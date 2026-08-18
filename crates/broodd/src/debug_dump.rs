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
    reexports::wayland_server::Resource,
    wayland::compositor::{BufferAssignment, SurfaceAttributes, with_states},
};

use crate::{buffer_readback, control, frame_export, state::Brood};

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
        crate::computer_use::capture_current(state)
            .map_err(anyhow::Error::msg)
            .context("refresh automation capture")?;
        let surfaces_dir = dir.join("surfaces");
        fs::create_dir_all(&surfaces_dir)
            .with_context(|| format!("create surfaces dir {}", surfaces_dir.display()))?;
        let surfaces = dump_surfaces(state, &surfaces_dir)?;
        manifest.insert("surfaces".to_string(), Value::Array(surfaces));
        if let Some(snapshot) = state.computer_use_automation.latest() {
            let png_path = dir.join("automation.png");
            fs::write(&png_path, &snapshot.png)
                .with_context(|| format!("write {}", png_path.display()))?;
            let metadata_path = dir.join("automation.json");
            fs::write(
                &metadata_path,
                serde_json::to_vec_pretty(&json!({
                    "viewport_width": brood_computer_use::AUTOMATION_VIEWPORT_WIDTH,
                    "viewport_height": brood_computer_use::AUTOMATION_VIEWPORT_HEIGHT,
                    "captured_at_ms": snapshot.captured_at_ms,
                    "frame_sequence": snapshot.frame_sequence,
                    "active_window": snapshot.active_window,
                    "windows": snapshot.windows,
                    "png_bytes": snapshot.png.len(),
                }))
                .context("encode automation metadata")?,
            )
            .with_context(|| format!("write {}", metadata_path.display()))?;
            manifest.insert(
                "automation".to_string(),
                json!({ "png": "automation.png", "metadata": "automation.json" }),
            );
        }
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
    let capture = match buffer_readback::read_shm_buffer(buffer) {
        Ok(Some(capture)) => capture,
        Ok(None) => return Ok(None),
        Err(err) => return Err(anyhow::anyhow!(err)),
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
