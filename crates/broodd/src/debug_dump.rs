#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::unnecessary_wraps,
    reason = "debug-dump callbacks retain callback-shaped mutable state"
)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use smithay::{
    backend::renderer::{buffer_dimensions, buffer_has_alpha, buffer_type},
    reexports::wayland_server::{Resource, protocol::wl_shm},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, with_states},
        shm::{BufferData, with_buffer_contents},
    },
};

use crate::{control, frame_export, state::Brood};

#[derive(Clone, Copy)]
pub struct DumpOptions {
    include_state: bool,
    include_surfaces: bool,
}

impl DumpOptions {
    pub fn parse(selector: Option<&str>) -> Result<Self> {
        let mut options = Self {
            include_state: true,
            include_surfaces: true,
        };

        let Some(selector) = selector else {
            return Ok(options);
        };
        if selector.trim().is_empty() || selector == "all" {
            return Ok(options);
        }

        options = Self {
            include_state: false,
            include_surfaces: false,
        };
        for token in selector
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            match token {
                "state" => options.include_state = true,
                "surfaces" | "buffers" => options.include_surfaces = true,
                other => bail!("unknown debug dump selector {other:?}"),
            }
        }
        Ok(options)
    }
}

pub fn dump_bundle(state: &mut Brood, dir: &Path, options: DumpOptions) -> Result<String> {
    fs::create_dir_all(dir).with_context(|| format!("create dump dir {}", dir.display()))?;

    let mut manifest = Map::new();
    manifest.insert(
        "selectors".to_string(),
        json!({
            "state": options.include_state,
            "surfaces": options.include_surfaces,
        }),
    );

    if options.include_state {
        let state_path = dir.join("state.json");
        let filtered = filtered_state_dump(state)?;
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&filtered).context("encode state dump")?,
        )
        .with_context(|| format!("write {}", state_path.display()))?;
        manifest.insert(
            "state".to_string(),
            json!({ "path": state_path.file_name().and_then(|s| s.to_str()).unwrap_or("state.json") }),
        );
    }

    if options.include_surfaces {
        let surfaces_dir = dir.join("surfaces");
        fs::create_dir_all(&surfaces_dir)
            .with_context(|| format!("create surfaces dir {}", surfaces_dir.display()))?;
        let surfaces = dump_surfaces(state, &surfaces_dir)?;
        manifest.insert("surfaces".to_string(), Value::Array(surfaces));
    }

    let manifest_path = dir.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&Value::Object(manifest)).context("encode dump manifest")?,
    )
    .with_context(|| format!("write {}", manifest_path.display()))?;

    Ok(manifest_path.display().to_string())
}

fn filtered_state_dump(state: &mut Brood) -> Result<Value> {
    let status = serde_json::from_str::<Value>(&control::status_json_string(state)?)
        .context("decode status snapshot")?;
    let windows = state
        .space
        .elements()
        .enumerate()
        .map(|(index, window)| {
            let marker_id = state
                .test_surface_markers
                .then(|| state.window_test_marker_id(window, index));
            let surface_id = Brood::window_toplevel_surface(window).map_or_else(
                || "none".to_string(),
                |surface| frame_export::sanitize_status_value(&format!("{:?}", surface.id())),
            );
            let size = window.geometry().size;
            json!({
                "index": index,
                "surface_id": surface_id,
                "marker_id": marker_id,
                "pane": state.window_pane_id(window),
                "source_size": { "w": size.w, "h": size.h },
                "focused": state.focused_window_index() == Some(index),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "status": status,
        "test_surface_markers": state.test_surface_markers,
        "windows": windows,
    }))
}

fn dump_surfaces(state: &mut Brood, dir: &Path) -> Result<Vec<Value>> {
    let mut surfaces = Vec::new();
    for (index, window) in state.space.elements().enumerate() {
        let Some(surface) = Brood::window_toplevel_surface(window) else {
            continue;
        };
        let marker_id = state
            .test_surface_markers
            .then(|| state.window_test_marker_id(window, index));
        let surface_id = frame_export::sanitize_status_value(&format!("{:?}", surface.id()));
        let buffer = with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            match guard.current().buffer.as_ref() {
                Some(BufferAssignment::NewBuffer(buffer)) => Some(buffer.clone()),
                _ => None,
            }
        });
        let Some(buffer) = buffer else {
            surfaces.push(json!({
                "index": index,
                "surface_id": surface_id,
                "marker_id": marker_id,
                "status": "no_current_buffer",
            }));
            continue;
        };

        let buffer_kind = format!("{:?}", buffer_type(&buffer));
        let dimensions = buffer_dimensions(&buffer)
            .map_or(Value::Null, |size| json!({ "w": size.w, "h": size.h }));
        let has_alpha = buffer_has_alpha(&buffer);

        let entry = match save_surface_buffer_png(&buffer, dir, &surface_id) {
            Ok(Some(path)) => json!({
                "index": index,
                "surface_id": surface_id,
                "marker_id": marker_id,
                "buffer_type": buffer_kind,
                "dimensions": dimensions,
                "has_alpha": has_alpha,
                "path": path.file_name().and_then(|s| s.to_str()).unwrap_or_default(),
                "status": "captured",
            }),
            Ok(None) => json!({
                "index": index,
                "surface_id": surface_id,
                "marker_id": marker_id,
                "buffer_type": buffer_kind,
                "dimensions": dimensions,
                "has_alpha": has_alpha,
                "status": "unsupported_buffer_for_png_dump",
            }),
            Err(err) => json!({
                "index": index,
                "surface_id": surface_id,
                "marker_id": marker_id,
                "buffer_type": buffer_kind,
                "dimensions": dimensions,
                "has_alpha": has_alpha,
                "status": "capture_failed",
                "error": err.to_string(),
            }),
        };
        surfaces.push(entry);
    }
    Ok(surfaces)
}

fn save_surface_buffer_png(
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    dir: &Path,
    surface_id: &str,
) -> Result<Option<PathBuf>> {
    let capture = with_buffer_contents(buffer, capture_shm_buffer)
        .map_err(|err| anyhow::anyhow!("read shm buffer: {err}"));
    let capture = match capture {
        Ok(Some(capture)) => capture,
        Ok(None) => return Ok(None),
        Err(err) => return Err(err),
    };
    let path = dir.join(format!("{surface_id}.png"));
    image::save_buffer(
        &path,
        &capture.bytes,
        capture.width,
        capture.height,
        image::ColorType::Rgba8,
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

struct SurfaceCapture {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

/// A validated view of one packed, four-byte-per-pixel SHM image.
///
/// Construction proves that every visible row fits inside the Smithay-provided
/// mapping. Consumers therefore operate on slices and complete pixels rather
/// than repeating offset, stride, and raw-pointer arithmetic.
struct ShmImageView<'a> {
    bytes: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl ShmImageView<'_> {
    /// Wrap the mapping passed to Smithay's `with_buffer_contents` callback.
    ///
    /// # Safety
    ///
    /// `ptr` must remain readable for `len` bytes for `'a`. This is precisely
    /// the callback contract of `with_buffer_contents`; the view never escapes
    /// that callback because it is converted into an owned `SurfaceCapture`.
    unsafe fn from_smithay_mapping(ptr: *const u8, len: usize, data: &BufferData) -> Option<Self> {
        let width = usize::try_from(data.width).ok()?;
        let height = usize::try_from(data.height).ok()?;
        let stride = usize::try_from(data.stride).ok()?;
        let offset = usize::try_from(data.offset).ok()?;
        if width == 0 || height == 0 || stride == 0 {
            return None;
        }
        let row_bytes = width.checked_mul(4)?;
        if stride < row_bytes {
            return None;
        }
        let span = if height == 0 {
            0
        } else {
            stride
                .checked_mul(height.checked_sub(1)?)?
                .checked_add(row_bytes)?
        };
        let end = offset.checked_add(span)?;
        let pointer_limit = usize::try_from(isize::MAX).ok()?;
        if end > len || end > pointer_limit || span > pointer_limit || ptr.is_null() {
            return None;
        }

        // SAFETY: The caller supplies Smithay's live SHM mapping. `offset` and
        // `span` were checked above to lie within its `len` readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), span) };
        Some(ShmImageView {
            bytes,
            width,
            height,
            stride,
        })
    }

    fn rgba(self, format: wl_shm::Format) -> Option<Vec<u8>> {
        let alpha = match format {
            wl_shm::Format::Argb8888 => true,
            wl_shm::Format::Xrgb8888 => false,
            _ => return None,
        };
        let row_bytes = self.width.checked_mul(4)?;
        let capacity = row_bytes.checked_mul(self.height)?;
        let mut rgba = Vec::with_capacity(capacity);

        for row in self.bytes.chunks(self.stride).take(self.height) {
            let visible = row.get(..row_bytes)?;
            for pixel in visible.chunks_exact(4) {
                let [blue, green, red, source_alpha] = pixel else {
                    return None;
                };
                rgba.extend_from_slice(&[
                    *red,
                    *green,
                    *blue,
                    if alpha { *source_alpha } else { 0xff },
                ]);
            }
        }
        (rgba.len() == capacity).then_some(rgba)
    }
}

fn capture_shm_buffer(ptr: *const u8, len: usize, data: BufferData) -> Option<SurfaceCapture> {
    // SAFETY: Smithay invokes this function only while `ptr..ptr+len` is a
    // readable mapping and keeps the mapping alive for the callback duration.
    let view = unsafe { ShmImageView::from_smithay_mapping(ptr, len, &data) }?;
    let rgba = view.rgba(data.format)?;
    Some(SurfaceCapture {
        width: u32::try_from(data.width).ok()?,
        height: u32::try_from(data.height).ok()?,
        bytes: rgba,
    })
}
