//! Narrow Linux DMA-BUF import boundary for the WGPU presentation client.

#![cfg(target_os = "linux")]

use std::os::fd::OwnedFd;

use anyhow::{Context, Result, ensure};
use wgpu_hal::api::Vulkan;

/// Metadata for the initial single-plane Linux fast path.
#[derive(Clone, Copy, Debug)]
pub struct SinglePlaneDmabuf {
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    /// Optional compatible view used to interpret the transported pixel
    /// encoding without changing the DMA-BUF's underlying bytes.
    pub view_format: Option<wgpu::TextureFormat>,
    pub modifier: u64,
    pub stride: u64,
    pub offset: u64,
}

/// Import one DMA-BUF plane into the existing Vulkan WGPU device.
///
/// The returned WGPU texture owns the imported Vulkan memory. Vulkan consumes
/// `fd` on successful import; the HAL closes it on failure.
pub fn import_single_plane(
    device: &wgpu::Device,
    fd: OwnedFd,
    metadata: SinglePlaneDmabuf,
) -> Result<wgpu::Texture> {
    ensure!(
        metadata.width > 0 && metadata.height > 0,
        "DMA-BUF extent must be nonzero"
    );
    ensure!(
        device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
        "WGPU device did not enable Vulkan DMA-BUF import"
    );

    let size = wgpu::Extent3d {
        width: metadata.width,
        height: metadata.height,
        depth_or_array_layers: 1,
    };
    let view_formats = metadata.view_format.into_iter().collect::<Vec<_>>();
    let hal_view_formats = view_formats.clone();
    let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
    let descriptor = wgpu::TextureDescriptor {
        label: Some("manifold imported DMA-BUF"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: metadata.format,
        usage,
        view_formats: &view_formats,
    };
    let hal_descriptor = wgpu_hal::TextureDescriptor {
        label: Some("manifold imported DMA-BUF"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: metadata.format,
        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
        memory_flags: wgpu_hal::MemoryFlags::empty(),
        view_formats: hal_view_formats,
    };

    // SAFETY: the guard keeps the exact Vulkan device behind `device` borrowed
    // while the HAL texture is created. `texture_from_dmabuf_fd` validates the
    // enabled feature and takes ownership of the supplied descriptor.
    let hal_texture = unsafe {
        let hal_device = device
            .as_hal::<Vulkan>()
            .context("WGPU device is not using the Vulkan backend")?;
        hal_device
            .texture_from_dmabuf_fd(
                fd,
                &hal_descriptor,
                metadata.modifier,
                metadata.stride,
                metadata.offset,
            )
            .context("import DMA-BUF into Vulkan")?
    };

    // SAFETY: `hal_texture` was created by this device with the matching
    // descriptor. It is newly wrapped, so WGPU performs its first transition
    // from the Vulkan image's initial state before use.
    Ok(unsafe {
        device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &descriptor,
            wgpu::TextureUses::UNINITIALIZED,
        )
    })
}
