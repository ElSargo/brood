//! Headless managed-window frame export for external presentation clients.
//!
//! This module owns no presentation window and performs no final layout. It
//! renders each managed Wayland toplevel into an independently exported
//! DMA-BUF, then publishes the resulting pane inventory over the presentation
//! fast path. Final placement, composition, hit testing, and capture belong to
//! `brood-client`.

#![allow(
    clippy::expect_used,
    clippy::wildcard_imports,
    reason = "Smithay's audited EGL construction and render-element APIs require framework-shaped unsafe and generic boundaries"
)]

use std::{
    cell::RefCell,
    env, fmt, fs,
    io::Error as IoError,
    mem::size_of,
    os::fd::OwnedFd,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::Result;
#[cfg(feature = "shm-fallback")]
use smithay::backend::renderer::ImportMemWl;
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Allocator, Buffer as AllocatorBuffer, Fourcc, Modifier,
            dmabuf::{Dmabuf, DmabufAllocator},
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDeviceFd, DrmNode},
        egl::{
            EGLContext, EGLDevice,
            context::{GlAttributes, PixelFormatRequirements},
            display::EGLDisplay,
            native,
        },
        renderer::{
            Bind, ExportMem, ImportDma, ImportEgl, Offscreen, RendererSuper,
            damage::OutputDamageTracker,
            element::solid::SolidColorRenderElement,
            gles::{GlesError, GlesRenderbuffer, GlesRenderer},
        },
    },
    desktop::{Space, Window},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    utils::{DeviceFd, Physical, Rectangle, Size, Transform},
    wayland::{
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
    },
};
use tracing::{info, warn};

use crate::state::{Brood, SESSION_OUTPUT_REFRESH_MILLIHZ};

type ExportAllocator = DmabufAllocator<GbmAllocator<DrmDeviceFd>>;
pub type SharedFrameExporter = Rc<RefCell<FrameExporter>>;

#[derive(serde::Serialize)]
struct CatalogSnapshot {
    active_session: u64,
    sessions: Vec<CatalogSession>,
    windows: Vec<CatalogWindow>,
}

#[derive(serde::Serialize)]
struct CatalogSession {
    id: u64,
    name: String,
    active: bool,
    windows: u64,
}

#[derive(serde::Serialize)]
struct CatalogWindow {
    pane: u64,
    session: u64,
    app_id: String,
}

#[derive(Debug)]
pub struct FrameExporter {
    renderer: GlesRenderer,
    _display: EGLDisplay,
    allocator: Option<ExportAllocator>,
}

impl FrameExporter {
    fn new() -> std::result::Result<Self, FrameExporterError> {
        let (display, allocator) = headless_egl_display()?;
        let attributes = GlAttributes {
            version: (3, 0),
            profile: None,
            debug: cfg!(debug_assertions),
            vsync: false,
        };
        let context = if env::var_os("BROOD_ALLOW_SOFTWARE_EGL_CONFIG").is_some() {
            EGLContext::new(&display)?
        } else {
            create_egl_context(&display, attributes)?
        };
        context.unbind()?;
        // SAFETY: the freshly-created context is unbound and moves uniquely
        // into the renderer. `_display` retains its native display owner for
        // at least as long as the renderer.
        let renderer = unsafe { GlesRenderer::new(context) }?;
        Ok(Self {
            renderer,
            _display: display,
            allocator,
        })
    }

    pub(crate) const fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    pub(crate) const fn supports_dmabuf_export(&self) -> bool {
        self.allocator.is_some()
    }

    fn create_exportable_dmabuf(
        &mut self,
        size: Size<i32, Physical>,
    ) -> std::result::Result<Dmabuf, FrameExporterError> {
        let allocator = self.allocator.as_mut().ok_or_else(|| {
            FrameExporterError::Headless(
                "headless renderer has no exportable DMA-BUF allocator".to_string(),
            )
        })?;
        let width = u32::try_from(size.w.max(1))
            .map_err(|error| FrameExporterError::Headless(error.to_string()))?;
        let height = u32::try_from(size.h.max(1))
            .map_err(|error| FrameExporterError::Headless(error.to_string()))?;
        allocator
            .create_buffer(width, height, Fourcc::Argb8888, &[Modifier::Linear])
            .map_err(|error| FrameExporterError::Headless(error.to_string()))
    }

    fn bind_dmabuf<'a>(
        &'a mut self,
        dmabuf: &'a mut Dmabuf,
    ) -> std::result::Result<
        (
            &'a mut GlesRenderer,
            <GlesRenderer as RendererSuper>::Framebuffer<'a>,
        ),
        SwapBuffersError,
    > {
        let framebuffer = self.renderer.bind(dmabuf)?;
        Ok((&mut self.renderer, framebuffer))
    }
}

/// Create the renderer used for managed-buffer import and per-pane export.
///
/// This is deliberately independent of external-client attachment. Hosted
/// Wayland clients need the renderer's DMA-BUF formats while the presentation
/// client is disconnected, and a later fast-path connection must not create a
/// backend OS window as a side effect.
pub fn initialize(state: &mut Brood) -> Result<()> {
    if state.dmabuf.exporter.is_some() {
        return Ok(());
    }
    let mut exporter = FrameExporter::new()?;

    #[cfg(feature = "shm-fallback")]
    {
        let shm_formats = exporter.renderer().shm_formats().collect::<Vec<_>>();
        state.dmabuf.shm_formats = shm_formats.len();
        state.shm_state.update_formats(shm_formats);
    }

    match exporter.renderer().bind_wl_display(&state.display_handle) {
        Ok(()) => {
            state.dmabuf.egl_wl_bind_status = "renderer".to_string();
            info!("EGL wl_display binding enabled through headless frame exporter");
        }
        Err(smithay::backend::egl::Error::EglExtensionNotSupported(extensions)) => {
            state.dmabuf.egl_wl_bind_status = "unsupported".to_string();
            info!(
                ?extensions,
                "legacy EGL wl_display binding unavailable; continuing with linux dmabuf"
            );
        }
        Err(error) => {
            state.dmabuf.egl_wl_bind_status = sanitize_status_value(&format!("{error:?}"));
            warn!(
                ?error,
                "failed to bind frame-export EGL display to Wayland display"
            );
        }
    }

    initialize_dmabuf(state, &mut exporter);
    state.dmabuf.exporter = Some(Rc::new(RefCell::new(exporter)));
    Ok(())
}

/// Export the current managed-window inventory when an external client is
/// attached. Backpressure is represented by the fast path's in-flight and
/// pending-redraw fields; no backend presentation target is involved.
pub fn request_redraw(state: &mut Brood) {
    if let Err(failure) = publish_catalog_snapshot(state) {
        record_failure(state, &failure);
    }
    let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
        return;
    };
    if fast_path.in_flight_sequence.is_some() {
        fast_path.pending_redraw = true;
        return;
    }
    let Some(exporter) = state.dmabuf.exporter.clone() else {
        record_failure(
            state,
            &FrameExportFailure::new("request_redraw", "frame exporter is not initialized"),
        );
        return;
    };
    let started = Instant::now();
    match render_external_client_scene(&mut exporter.borrow_mut(), state) {
        Ok(()) => state.stats.record_frame(started.elapsed()),
        Err(failure) => record_failure(state, &failure),
    }
}

/// Publish the latest session and managed-window catalog over its dedicated
/// shared-memory lane. Identical latest state is coalesced.
pub fn publish_catalog_snapshot(state: &mut Brood) -> std::result::Result<(), FrameExportFailure> {
    if state.dmabuf.fast_path.is_none() {
        return Ok(());
    }
    let payload = CatalogSnapshot {
        active_session: state.active_surface_session().raw(),
        sessions: state
            .surface_session_summaries()
            .into_iter()
            .map(|session| CatalogSession {
                id: session.id,
                name: session.name,
                active: session.active,
                windows: u64::try_from(session.windows).unwrap_or(u64::MAX),
            })
            .collect(),
        windows: state
            .presentation_window_summaries()
            .into_iter()
            .map(|window| CatalogWindow {
                pane: window.pane,
                session: window.session,
                app_id: window.app_id,
            })
            .collect(),
    };
    let payload = serde_json::to_vec(&payload)
        .map_err(|error| FrameExportFailure::new("encode_catalog_snapshot", error))?;
    let chunk_count = payload.len().div_ceil(brood_ipc::CATALOG_CHUNK_BYTES);
    if chunk_count.saturating_add(2) > brood_ipc::CRITICAL_CAPACITY {
        return Err(FrameExportFailure::new(
            "publish_catalog_snapshot",
            format!(
                "encoded catalog requires {} records but the lane holds {}",
                chunk_count.saturating_add(2),
                brood_ipc::CRITICAL_CAPACITY
            ),
        ));
    }

    let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
        return Ok(());
    };
    if fast_path.last_catalog_payload == payload {
        return Ok(());
    }
    let revision = fast_path.catalog_revision.wrapping_add(1);
    let publish_result = (|| {
        fast_path
            .catalog
            .try_send(brood_ipc::CriticalRecord {
                opcode: brood_ipc::CATALOG_SNAPSHOT_BEGIN,
                connection_generation: fast_path.generation,
                sequence: revision,
                arguments: [
                    u64::try_from(payload.len()).unwrap_or(u64::MAX),
                    u64::try_from(chunk_count).unwrap_or(u64::MAX),
                    0,
                    0,
                    0,
                ],
                ..brood_ipc::CriticalRecord::default()
            })
            .map_err(|record| FrameExportFailure::new("publish_catalog_begin", record))?;
        for chunk in payload.chunks(brood_ipc::CATALOG_CHUNK_BYTES) {
            let mut arguments = [0_u64; 5];
            for (word, bytes) in arguments.iter_mut().zip(chunk.chunks(size_of::<u64>())) {
                let mut encoded = [0_u8; size_of::<u64>()];
                if let Some(target) = encoded.get_mut(..bytes.len()) {
                    target.copy_from_slice(bytes);
                }
                *word = u64::from_ne_bytes(encoded);
            }
            fast_path
                .catalog
                .try_send(brood_ipc::CriticalRecord {
                    opcode: brood_ipc::CATALOG_SNAPSHOT_CHUNK,
                    flags: u32::try_from(chunk.len()).unwrap_or(u32::MAX),
                    connection_generation: fast_path.generation,
                    sequence: revision,
                    arguments,
                })
                .map_err(|record| FrameExportFailure::new("publish_catalog_chunk", record))?;
        }
        fast_path
            .catalog
            .try_send(brood_ipc::CriticalRecord {
                opcode: brood_ipc::CATALOG_SNAPSHOT_END,
                connection_generation: fast_path.generation,
                sequence: revision,
                ..brood_ipc::CriticalRecord::default()
            })
            .map_err(|record| FrameExportFailure::new("publish_catalog_end", record))
    })();
    fast_path
        .catalog_notifications
        .notify()
        .map_err(|error| FrameExportFailure::new("notify_catalog_snapshot", error))?;
    publish_result?;
    fast_path.catalog_revision = revision;
    fast_path.last_catalog_payload = payload;
    Ok(())
}

#[derive(Clone)]
struct ExternalWindowFrame {
    window_id: u64,
    app_id: String,
    source_x: i32,
    source_y: i32,
    source_w: i32,
    source_h: i32,
    active: bool,
    window: Window,
}

enum RenderedWindowBacking {
    Dmabuf(Dmabuf),
    SharedMemory(Vec<u8>),
}

fn render_external_client_scene(
    exporter: &mut FrameExporter,
    state: &mut Brood,
) -> std::result::Result<(), FrameExportFailure> {
    let frame_transport = state
        .dmabuf
        .fast_path
        .as_ref()
        .map(|fast_path| fast_path.profile.frames)
        .ok_or_else(|| {
            FrameExportFailure::new("render_window_scene", "fast path is unavailable")
        })?;
    let active_window = state.active_presentation_client_window();
    let mut windows = state
        .space
        .elements()
        .filter(|window| state.window_available_to_presentation_client(window))
        .enumerate()
        .filter_map(|(index, window)| {
            let source_size = window.geometry().size;
            (source_size.w > 0 && source_size.h > 0).then(|| ExternalWindowFrame {
                window_id: state
                    .window_pane_id(window)
                    .unwrap_or_else(|| state.window_test_marker_id(window, index)),
                app_id: state.window_app_id(window).unwrap_or_default(),
                source_x: 0,
                source_y: 0,
                source_w: source_size.w,
                source_h: source_size.h,
                active: active_window.as_ref() == Some(window),
                window: window.clone(),
            })
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|window| window.active);

    if windows.len().saturating_add(2) > brood_ipc::CRITICAL_CAPACITY {
        return Err(FrameExportFailure::new(
            "publish_window_scene",
            format!(
                "scene has {} windows but the transport batch holds at most {}",
                windows.len(),
                brood_ipc::CRITICAL_CAPACITY.saturating_sub(2)
            ),
        ));
    }

    let mut rendered = Vec::with_capacity(windows.len());
    for window in windows {
        let size: Size<i32, Physical> = (window.source_w.max(1), window.source_h.max(1)).into();
        let backing = match frame_transport {
            brood_protocol::FrameTransportKind::Dmabuf => {
                let existing = state
                    .dmabuf
                    .fast_path
                    .as_mut()
                    .and_then(|fast_path| fast_path.window_buffers.remove(&window.window_id));
                let mut dmabuf = match existing {
                    Some(buffer) if buffer.size() == (size.w, size.h).into() => buffer,
                    _ => exporter.create_exportable_dmabuf(size).map_err(|error| {
                        FrameExportFailure::new("allocate_window_dmabuf", error)
                    })?,
                };
                render_window_dmabuf(exporter, state, &window, &mut dmabuf)?;
                let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
                    return Err(FrameExportFailure::new(
                        "store_window_dmabuf",
                        "fast path closed while rendering",
                    ));
                };
                fast_path
                    .window_buffers
                    .insert(window.window_id, dmabuf.clone());
                RenderedWindowBacking::Dmabuf(dmabuf)
            }
            brood_protocol::FrameTransportKind::SharedMemory => {
                RenderedWindowBacking::SharedMemory(render_window_shared_memory(
                    exporter, state, &window,
                )?)
            }
            brood_protocol::FrameTransportKind::Network => {
                return Err(FrameExportFailure::new(
                    "render_window_scene",
                    "network frame transport reached local exporter",
                ));
            }
        };
        rendered.push((window, backing));
    }

    let scene_size: Size<i32, Physical> = state
        .session_output
        .current_mode()
        .map_or_else(|| (1280, 800).into(), |mode| mode.size);
    publish_window_scene(state, scene_size, rendered)
}

fn render_window_dmabuf(
    exporter: &mut FrameExporter,
    state: &Brood,
    window: &ExternalWindowFrame,
    dmabuf: &mut Dmabuf,
) -> std::result::Result<(), FrameExportFailure> {
    let size: Size<i32, Physical> = (window.source_w.max(1), window.source_h.max(1)).into();
    let (output, window_space) = window_render_space(state, window, size);
    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    let sync = {
        let (renderer, mut framebuffer) = exporter
            .bind_dmabuf(dmabuf)
            .map_err(|error| FrameExportFailure::new("bind_window_dmabuf", error))?;
        let sync = smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
            &output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&window_space],
            &[],
            &mut damage_tracker,
            [0.02, 0.02, 0.025, 1.0],
        )
        .map_err(|error| FrameExportFailure::new("render_window_dmabuf", error))?
        .sync;
        sync.wait()
            .map_err(|error| FrameExportFailure::new("wait_window_dmabuf", error))?;
        sync
    };
    drop(sync);
    Ok(())
}

fn render_window_shared_memory(
    exporter: &mut FrameExporter,
    state: &Brood,
    window: &ExternalWindowFrame,
) -> std::result::Result<Vec<u8>, FrameExportFailure> {
    let size: Size<i32, Physical> = (window.source_w.max(1), window.source_h.max(1)).into();
    let (output, window_space) = window_render_space(state, window, size);
    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut renderbuffer = Offscreen::<GlesRenderbuffer>::create_buffer(
        exporter.renderer(),
        Fourcc::Abgr8888,
        (size.w, size.h).into(),
    )
    .map_err(|error| FrameExportFailure::new("allocate_window_shared_memory", error))?;

    let renderer = exporter.renderer();
    let mut framebuffer = renderer
        .bind(&mut renderbuffer)
        .map_err(|error| FrameExportFailure::new("bind_window_shared_memory", error))?;
    let sync = smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
        &output,
        renderer,
        &mut framebuffer,
        1.0,
        0,
        [&window_space],
        &[],
        &mut damage_tracker,
        [0.02, 0.02, 0.025, 1.0],
    )
    .map_err(|error| FrameExportFailure::new("render_window_shared_memory", error))?
    .sync;
    sync.wait()
        .map_err(|error| FrameExportFailure::new("wait_window_shared_memory", error))?;
    let mapping = renderer
        .copy_framebuffer(
            &framebuffer,
            Rectangle::from_size((size.w, size.h).into()),
            Fourcc::Abgr8888,
        )
        .map_err(|error| FrameExportFailure::new("copy_window_shared_memory", error))?;
    drop(framebuffer);
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|error| FrameExportFailure::new("map_window_shared_memory", error))?
        .to_vec();
    drop(sync);
    Ok(pixels)
}

fn window_render_space(
    state: &Brood,
    window: &ExternalWindowFrame,
    size: Size<i32, Physical>,
) -> (Output, Space<Window>) {
    let output = Output::new(
        format!("brood-window-{}", window.window_id),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Brood".into(),
            model: "External client window frame".into(),
        },
    );
    let mode = Mode {
        size,
        refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(state.session_output.current_scale()),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let mut window_space = Space::default();
    window_space.map_output(&output, (0, 0));
    window_space.map_element(window.window.clone(), (0, 0), true);
    (output, window_space)
}

fn publish_window_scene(
    state: &mut Brood,
    scene_size: Size<i32, Physical>,
    windows: Vec<(ExternalWindowFrame, RenderedWindowBacking)>,
) -> std::result::Result<(), FrameExportFailure> {
    state.dmabuf.latest_client_buffer_sequence =
        state.dmabuf.latest_client_buffer_sequence.wrapping_add(1);
    let sequence = state.dmabuf.latest_client_buffer_sequence;
    let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
        return Ok(());
    };
    if fast_path.in_flight_sequence.is_some() {
        fast_path.pending_redraw = true;
        return Ok(());
    }
    fast_path
        .frames
        .try_send(brood_ipc::CriticalRecord {
            opcode: brood_ipc::FRAME_SCENE_BEGIN,
            connection_generation: fast_path.generation,
            sequence,
            arguments: [
                u64::try_from(windows.len()).unwrap_or(u64::MAX),
                u64::try_from(scene_size.w.max(1)).unwrap_or(u64::MAX),
                u64::try_from(scene_size.h.max(1)).unwrap_or(u64::MAX),
                0,
                0,
            ],
            ..brood_ipc::CriticalRecord::default()
        })
        .map_err(|error| FrameExportFailure::new("publish_scene_begin", error))?;

    for (window, backing) in windows {
        let (opcode, resource_id) = match backing {
            RenderedWindowBacking::Dmabuf(dmabuf) => {
                let weak_dmabuf = dmabuf.weak();
                let resource_id = if let Some(id) = fast_path.resources.get(&weak_dmabuf) {
                    *id
                } else {
                    fast_path.resources.retain(|buffer, _| !buffer.is_gone());
                    let id = fast_path.next_resource_id;
                    fast_path.next_resource_id = id.wrapping_add(1);
                    fast_path.resources.insert(weak_dmabuf, id);
                    fast_path.buffers.insert(id, dmabuf);
                    fast_path.resource_app_ids.insert(id, window.app_id.clone());
                    id
                };
                (brood_ipc::FRAME_WINDOW_DMABUF, resource_id)
            }
            RenderedWindowBacking::SharedMemory(pixels) => {
                let width = u32::try_from(window.source_w.max(1))
                    .map_err(|error| FrameExportFailure::new("shared_image_width", error))?;
                let height = u32::try_from(window.source_h.max(1))
                    .map_err(|error| FrameExportFailure::new("shared_image_height", error))?;
                let stride = width.checked_mul(4).ok_or_else(|| {
                    FrameExportFailure::new("shared_image_stride", "stride overflow")
                })?;
                let metadata = brood_ipc::SharedImageMetadata {
                    width,
                    height,
                    stride,
                    pixel_format: brood_protocol::FramePixelFormat::Rgba8,
                    color_space: brood_protocol::FrameColorSpace::Srgb,
                };
                let existing = fast_path
                    .window_shared_image_resources
                    .get(&window.window_id)
                    .copied()
                    .filter(|resource| {
                        fast_path
                            .shared_images
                            .get(resource)
                            .is_some_and(|(_, image)| image.metadata() == metadata)
                    });
                let resource_id = if let Some(resource) = existing {
                    resource
                } else {
                    if let Some(stale) = fast_path
                        .window_shared_image_resources
                        .remove(&window.window_id)
                    {
                        fast_path.shared_images.remove(&stale);
                    }
                    let resource = fast_path.next_resource_id;
                    fast_path.next_resource_id = resource.wrapping_add(1);
                    let image =
                        brood_ipc::SharedImageProducer::create(fast_path.generation, metadata)
                            .map_err(|error| {
                                FrameExportFailure::new("create_shared_image", error)
                            })?;
                    fast_path
                        .shared_images
                        .insert(resource, (window.app_id.clone(), image));
                    fast_path
                        .window_shared_image_resources
                        .insert(window.window_id, resource);
                    resource
                };
                let (_, image) =
                    fast_path
                        .shared_images
                        .get_mut(&resource_id)
                        .ok_or_else(|| {
                            FrameExportFailure::new(
                                "publish_shared_image",
                                "shared image disappeared before publication",
                            )
                        })?;
                image
                    .publish(sequence, &pixels)
                    .map_err(|error| FrameExportFailure::new("publish_shared_image", error))?;
                (brood_ipc::FRAME_WINDOW_SHM, resource_id)
            }
        };
        fast_path
            .frames
            .try_send(brood_ipc::CriticalRecord {
                opcode,
                connection_generation: fast_path.generation,
                sequence,
                arguments: [
                    window.window_id,
                    resource_id,
                    pack_i32_pair(window.source_x, window.source_y),
                    pack_i32_pair(window.source_w, window.source_h),
                    u64::from(window.active),
                ],
                ..brood_ipc::CriticalRecord::default()
            })
            .map_err(|error| FrameExportFailure::new("publish_window_frame", error))?;
    }
    fast_path
        .frames
        .try_send(brood_ipc::CriticalRecord {
            opcode: brood_ipc::FRAME_SCENE_END,
            connection_generation: fast_path.generation,
            sequence,
            ..brood_ipc::CriticalRecord::default()
        })
        .map_err(|error| FrameExportFailure::new("publish_scene_end", error))?;
    fast_path
        .frame_notifications
        .notify()
        .map_err(|error| FrameExportFailure::new("notify_window_scene", error))?;
    fast_path.in_flight_sequence = Some(sequence);
    Ok(())
}

fn pack_i32_pair(first: i32, second: i32) -> u64 {
    u64::from(u32::from_ne_bytes(first.to_ne_bytes()))
        | (u64::from(u32::from_ne_bytes(second.to_ne_bytes())) << 32)
}

pub fn send_external_client_frame_callbacks(state: &mut Brood) -> u64 {
    let output = state.session_output.clone();
    let active_window = state.active_presentation_client_window();
    let active_surface = active_window
        .as_ref()
        .and_then(Brood::window_toplevel_surface);
    let mut windows = Vec::new();
    if let Some(active_window) = active_window {
        windows.push(active_window);
    }
    windows.extend(
        state
            .space
            .elements()
            .filter(|window| state.window_available_to_presentation_client(window))
            .filter(|window| {
                active_surface.as_ref() != Brood::window_toplevel_surface(window).as_ref()
            })
            .cloned(),
    );

    let mut callbacks = 0;
    for window in &windows {
        window.send_frame(
            &output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
        callbacks += 1;
        if let Some(feedback) = state.dmabuf.default_feedback.as_ref() {
            window.send_dmabuf_feedback(&output, |_, _| Some(output.clone()), |_, _| feedback);
        }
    }
    state.stats.record_client_frame_callbacks(callbacks);
    if let Err(error) = state.display_handle.flush_clients() {
        warn!(
            ?error,
            "flush clients after external frame callbacks failed"
        );
    }
    callbacks
}

fn initialize_dmabuf(state: &mut Brood, exporter: &mut FrameExporter) {
    let mut formats = exporter.renderer().dmabuf_formats();
    if env::var_os("BROOD_DMABUF_HAPPY_PATH").is_some() {
        formats = formats
            .iter()
            .filter(|format| {
                format.modifier == Modifier::Linear || format.modifier == Modifier::Invalid
            })
            .copied()
            .collect();
    }
    state.dmabuf.advertised_formats = formats.indexset().len();
    if state.dmabuf.advertised_formats == 0 {
        disable_dmabuf(state, "no_dmabuf_texture_formats");
        #[cfg(not(feature = "shm-fallback"))]
        panic!("frame exporter reported no importable dmabuf texture formats");
        #[cfg(feature = "shm-fallback")]
        warn!("frame exporter reported no dmabuf texture formats; linux dmabuf global disabled");
        return;
    }
    state.dmabuf.disabled_reason = None;

    let render_node = EGLDevice::device_for_display(exporter.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    match render_node {
        Ok(Some(node)) => {
            let feedback = match DmabufFeedbackBuilder::new(node.dev_id(), formats).build() {
                Ok(feedback) => feedback,
                Err(error) => {
                    state.dmabuf.render_node = Some(format!("{node:?}"));
                    disable_dmabuf(state, "default_feedback_error");
                    warn!(
                        ?error,
                        render_node = ?state.dmabuf.render_node,
                        "failed to build linux dmabuf default feedback"
                    );
                    return;
                }
            };
            let global = state
                .dmabuf
                .state
                .create_global_with_default_feedback::<Brood>(&state.display_handle, &feedback);
            state.dmabuf.global = Some(global);
            state.dmabuf.default_feedback = Some(feedback);
            state.dmabuf.protocol_version = Some(4);
            state.dmabuf.render_node = Some(format!("{node:?}"));
            initialize_explicit_sync(state, node);
            info!(
                render_node = ?state.dmabuf.render_node,
                formats = state.dmabuf.advertised_formats,
                "linux dmabuf v4 advertised by headless frame exporter"
            );
        }
        Ok(None) => {
            disable_dmabuf(state, "no_render_node");
            warn!("failed to query frame-export EGL render node");
        }
        Err(error) => {
            disable_dmabuf(state, "egl_device_error");
            warn!(?error, "failed to query frame-export EGL device");
        }
    }
}

fn initialize_explicit_sync(state: &mut Brood, render_node: DrmNode) {
    let Some(path) = render_node.dev_path() else {
        state.dmabuf.explicit_sync_disabled_reason = "drm_render_node_path_unavailable";
        return;
    };
    let file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) => {
            state.dmabuf.explicit_sync_disabled_reason = "drm_render_node_open_failed";
            warn!(?error, path = %path.display(), "failed to open frame-export render node");
            return;
        }
    };
    let import_device = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));
    if !supports_syncobj_eventfd(&import_device) {
        state.dmabuf.explicit_sync_disabled_reason = "drm_syncobj_eventfd_unsupported";
        return;
    }
    state.dmabuf.explicit_sync_state = Some(DrmSyncobjState::new::<Brood>(
        &state.display_handle,
        import_device,
    ));
    state.dmabuf.explicit_sync_protocol_version = Some(1);
    state.dmabuf.explicit_sync_disabled_reason = "none";
}

fn disable_dmabuf(state: &mut Brood, reason: &'static str) {
    state.dmabuf.global = None;
    state.dmabuf.default_feedback = None;
    state.dmabuf.protocol_version = None;
    state.dmabuf.disabled_reason = Some(reason);
    state.dmabuf.explicit_sync_state = None;
    state.dmabuf.explicit_sync_protocol_version = None;
    state.dmabuf.explicit_sync_disabled_reason = reason;
}

fn create_egl_context(
    display: &EGLDisplay,
    attributes: GlAttributes,
) -> std::result::Result<EGLContext, FrameExporterError> {
    let mut ten_bit = PixelFormatRequirements::_10_bit();
    let mut eight_bit = PixelFormatRequirements::_8_bit();
    if env::var_os("BROOD_ALLOW_SOFTWARE_EGL_CONFIG").is_some() {
        ten_bit.hardware_accelerated = None;
        eight_bit.hardware_accelerated = None;
    }
    Ok(EGLContext::new_with_config(display, attributes, ten_bit)
        .or_else(|_| EGLContext::new_with_config(display, attributes, eight_bit))?)
}

fn headless_egl_display()
-> std::result::Result<(EGLDisplay, Option<ExportAllocator>), FrameExporterError> {
    let mut failures = Vec::new();
    for path in render_node_candidates() {
        match gbm_egl_display_from_path(&path) {
            Ok((display, allocator)) => {
                return Ok((display, Some(DmabufAllocator(allocator))));
            }
            Err(error) => failures.push(format!("{}:{error}", path.display())),
        }
    }

    // SAFETY: Smithay's zero-sized native handle represents EGL's surfaceless
    // platform and owns no external pointer or borrowed lifetime.
    match unsafe { EGLDisplay::new(native::EGLSurfacelessDisplay) } {
        Ok(display) => Ok((display, None)),
        Err(error) if failures.is_empty() => Err(error.into()),
        Err(error) => Err(FrameExporterError::Headless(format!(
            "gbm render-node attempts failed [{}]; surfaceless fallback failed: {error}",
            failures.join(", ")
        ))),
    }
}

fn gbm_egl_display_from_path(
    path: &PathBuf,
) -> std::result::Result<(EGLDisplay, GbmAllocator<DrmDeviceFd>), FrameExporterError> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));
    let gbm = GbmDevice::new(drm_fd)?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR,
    );
    // SAFETY: ownership of the GBM device and its DRM fd moves into
    // `EGLDisplay`; neither can be closed while EGL uses them.
    let display = unsafe { EGLDisplay::new(gbm) }?;
    Ok((display, allocator))
}

fn render_node_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("BROOD_RENDER_NODE") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(devices) = EGLDevice::enumerate() {
        for device in devices {
            if let Ok(path) = device.render_device_path() {
                candidates.push(path);
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/dev/dri") {
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        candidates.extend(paths);
    }
    candidates.dedup();
    candidates
}

fn record_failure(state: &mut Brood, failure: &FrameExportFailure) {
    state.dmabuf.stats.rejected = state.dmabuf.stats.rejected.saturating_add(1);
    state.dmabuf.stats.last_error = Some(failure.status_value());
    warn!(
        context = failure.context,
        error = %failure.detail,
        "external managed-window frame export failed"
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameExportFailure {
    context: &'static str,
    detail: String,
}

impl FrameExportFailure {
    fn new(context: &'static str, error: impl fmt::Debug) -> Self {
        Self {
            context,
            detail: format!("{error:?}"),
        }
    }

    fn status_value(&self) -> String {
        sanitize_status_value(&format!("frame_export_{}:{}", self.context, self.detail))
    }
}

#[derive(Debug, thiserror::Error)]
enum FrameExporterError {
    #[error("create EGL display or context: {0}")]
    Egl(#[from] smithay::backend::egl::Error),
    #[error("unbind EGL context: {0}")]
    MakeCurrent(#[from] smithay::backend::egl::MakeCurrentError),
    #[error("create GLES renderer: {0}")]
    Renderer(#[from] GlesError),
    #[error("create GBM frame-export display: {0}")]
    Gbm(#[from] IoError),
    #[error("create headless frame exporter: {0}")]
    Headless(String),
}

pub fn sanitize_status_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '[' | ']' | '"') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
