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

use crate::{control, presentation, state::Manifold};

#[derive(Clone, Copy)]
pub struct DumpOptions {
    include_state: bool,
    include_composed: bool,
    include_surfaces: bool,
}

impl DumpOptions {
    pub fn parse(selector: Option<&str>) -> Result<Self> {
        let mut options = Self {
            include_state: true,
            include_composed: true,
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
            include_composed: false,
            include_surfaces: false,
        };
        for token in selector
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            match token {
                "state" => options.include_state = true,
                "composed" | "final" => options.include_composed = true,
                "surfaces" | "buffers" => options.include_surfaces = true,
                other => bail!("unknown debug dump selector {other:?}"),
            }
        }
        Ok(options)
    }
}

pub fn dump_bundle(state: &mut Manifold, dir: &Path, options: DumpOptions) -> Result<String> {
    fs::create_dir_all(dir).with_context(|| format!("create dump dir {}", dir.display()))?;

    let mut manifest = Map::new();
    manifest.insert(
        "selectors".to_string(),
        json!({
            "state": options.include_state,
            "composed": options.include_composed,
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

    if options.include_composed {
        let composed_path = dir.join("composed.png");
        presentation::dump_composed_frame(state, &composed_path)
            .map_err(|failure| anyhow::anyhow!("presentation capture failed: {:?}", failure))?;
        manifest.insert(
            "composed".to_string(),
            json!({ "path": composed_path.file_name().and_then(|s| s.to_str()).unwrap_or("composed.png") }),
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

fn filtered_state_dump(state: &mut Manifold) -> Result<Value> {
    let status = serde_json::from_str::<Value>(&control::status_json_string(state)?)
        .context("decode status snapshot")?;
    let windows = state
        .space
        .elements()
        .enumerate()
        .map(|(index, window)| {
            let surface_id = Manifold::window_toplevel_surface(window)
                .map(|surface| {
                    crate::presentation::sanitize_status_value(&format!("{:?}", surface.id()))
                })
                .unwrap_or_else(|| "none".to_string());
            let rect = state
                .window_layout_rect(window)
                .map(|rect| json!({ "x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h }));
            json!({
                "index": index,
                "surface_id": surface_id,
                "rect": rect,
                "focused": state.focused_window_index() == Some(index),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "status": status,
        "windows": windows,
        "layout": {
            "active_rect": state.active_layout_rect().map(|rect| json!({
                "x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h
            })),
            "managed_fullscreen": state.managed_fullscreen(),
        }
    }))
}

fn dump_surfaces(state: &mut Manifold, dir: &Path) -> Result<Vec<Value>> {
    let mut surfaces = Vec::new();
    for (index, window) in state.space.elements().enumerate() {
        let Some(surface) = Manifold::window_toplevel_surface(window) else {
            continue;
        };
        let surface_id = crate::presentation::sanitize_status_value(&format!("{:?}", surface.id()));
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
                "status": "no_current_buffer",
            }));
            continue;
        };

        let buffer_kind = format!("{:?}", buffer_type(&buffer));
        let dimensions = buffer_dimensions(&buffer)
            .map(|size| json!({ "w": size.w, "h": size.h }))
            .unwrap_or(Value::Null);
        let has_alpha = buffer_has_alpha(&buffer);

        let entry = match save_surface_buffer_png(&buffer, dir, &surface_id) {
            Ok(Some(path)) => json!({
                "index": index,
                "surface_id": surface_id,
                "buffer_type": buffer_kind,
                "dimensions": dimensions,
                "has_alpha": has_alpha,
                "path": path.file_name().and_then(|s| s.to_str()).unwrap_or_default(),
                "status": "captured",
            }),
            Ok(None) => json!({
                "index": index,
                "surface_id": surface_id,
                "buffer_type": buffer_kind,
                "dimensions": dimensions,
                "has_alpha": has_alpha,
                "status": "unsupported_buffer_for_png_dump",
            }),
            Err(err) => json!({
                "index": index,
                "surface_id": surface_id,
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
    let capture = with_buffer_contents(buffer, |ptr, len, data| capture_shm_buffer(ptr, len, data))
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
        capture.width as u32,
        capture.height as u32,
        image::ColorType::Rgba8,
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

struct SurfaceCapture {
    width: i32,
    height: i32,
    bytes: Vec<u8>,
}

fn capture_shm_buffer(ptr: *const u8, len: usize, data: BufferData) -> Option<SurfaceCapture> {
    let width = usize::try_from(data.width).ok()?;
    let height = usize::try_from(data.height).ok()?;
    let stride = usize::try_from(data.stride).ok()?;
    let offset = usize::try_from(data.offset).ok()?;
    let bytes_per_pixel = 4;
    let needed = offset
        .checked_add(stride.checked_mul(height.saturating_sub(1))?)?
        .checked_add(width.checked_mul(bytes_per_pixel)?)?;
    if needed > len {
        return None;
    }
    let src = unsafe { std::slice::from_raw_parts(ptr.add(offset), len - offset) };
    let mut rgba = vec![0_u8; width.checked_mul(height)?.checked_mul(4)?];
    match data.format {
        wl_shm::Format::Argb8888 => {
            for y in 0..height {
                let row = &src[y * stride..y * stride + width * 4];
                for x in 0..width {
                    let src_px = &row[x * 4..x * 4 + 4];
                    let dst_px = &mut rgba[(y * width + x) * 4..(y * width + x + 1) * 4];
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                    dst_px[3] = src_px[3];
                }
            }
        }
        wl_shm::Format::Xrgb8888 => {
            for y in 0..height {
                let row = &src[y * stride..y * stride + width * 4];
                for x in 0..width {
                    let src_px = &row[x * 4..x * 4 + 4];
                    let dst_px = &mut rgba[(y * width + x) * 4..(y * width + x + 1) * 4];
                    dst_px[0] = src_px[2];
                    dst_px[1] = src_px[1];
                    dst_px[2] = src_px[0];
                    dst_px[3] = 0xff;
                }
            }
        }
        _ => return None,
    }
    Some(SurfaceCapture {
        width: data.width,
        height: data.height,
        bytes: rgba,
    })
}
