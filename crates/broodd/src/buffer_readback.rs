//! Reusable server-side pixel readback.
//!
//! SHM buffers can be copied directly. DMA-BUF and SHM application surfaces
//! can both be imported by the headless GLES renderer; callers render those
//! surfaces into an offscreen framebuffer and use [`read_gles_framebuffer`]
//! for the bounded CPU copy.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{ExportMem, RendererSuper, gles::GlesRenderer},
    },
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    utils::{Physical, Rectangle, Size},
    wayland::shm::{BufferData, with_buffer_contents},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

pub fn read_gles_framebuffer(
    renderer: &mut GlesRenderer,
    framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
) -> Result<RgbaFrame, String> {
    let mapping = renderer
        .copy_framebuffer(
            framebuffer,
            Rectangle::from_size((size.w, size.h).into()),
            Fourcc::Abgr8888,
        )
        .map_err(|error| format!("copy framebuffer: {error:?}"))?;
    let bytes = renderer
        .map_texture(&mapping)
        .map_err(|error| format!("map framebuffer: {error:?}"))?
        .to_vec();
    Ok(RgbaFrame {
        width: u32::try_from(size.w).map_err(|error| error.to_string())?,
        height: u32::try_from(size.h).map_err(|error| error.to_string())?,
        bytes,
    })
}

pub fn read_shm_buffer(buffer: &WlBuffer) -> Result<Option<RgbaFrame>, String> {
    with_buffer_contents(buffer, capture_shm_buffer)
        .map_err(|error| format!("read shm buffer: {error}"))
}

struct ShmImageView<'a> {
    bytes: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl ShmImageView<'_> {
    unsafe fn from_smithay_mapping(ptr: *const u8, len: usize, data: &BufferData) -> Option<Self> {
        let width = usize::try_from(data.width).ok()?;
        let height = usize::try_from(data.height).ok()?;
        let stride = usize::try_from(data.stride).ok()?;
        let offset = usize::try_from(data.offset).ok()?;
        if width == 0 || height == 0 || stride < width.checked_mul(4)? {
            return None;
        }
        let row_bytes = width.checked_mul(4)?;
        let span = stride
            .checked_mul(height.checked_sub(1)?)?
            .checked_add(row_bytes)?;
        let end = offset.checked_add(span)?;
        let pointer_limit = usize::try_from(isize::MAX).ok()?;
        if ptr.is_null() || end > len || end > pointer_limit || span > pointer_limit {
            return None;
        }
        // SAFETY: Smithay owns the live mapping for the callback duration and
        // the checked offset/span are wholly inside it.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), span) };
        Some(Self {
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
            for pixel in row.get(..row_bytes)?.chunks_exact(4) {
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

fn capture_shm_buffer(ptr: *const u8, len: usize, data: BufferData) -> Option<RgbaFrame> {
    // SAFETY: this callback is invoked only while Smithay keeps the mapping
    // readable; the view is immediately copied into an owned vector.
    let view = unsafe { ShmImageView::from_smithay_mapping(ptr, len, &data) }?;
    Some(RgbaFrame {
        width: u32::try_from(data.width).ok()?,
        height: u32::try_from(data.height).ok()?,
        bytes: view.rgba(data.format)?,
    })
}
