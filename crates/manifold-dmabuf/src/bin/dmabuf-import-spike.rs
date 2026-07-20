use std::{fs::OpenOptions, path::PathBuf, sync::mpsc};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use gbm::{BufferObjectFlags, Device as GbmDevice, Format};
use manifold_dmabuf::{SinglePlaneDmabuf, import_single_plane};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const BYTES_PER_ROW: u32 = WIDTH * 4;
const EXPECTED_BGRA: [u8; 4] = [0x11, 0x22, 0xcc, 0xff];

#[derive(Debug, Parser)]
#[command(about = "Prove zero-copy DMA-BUF import into the Vulkan WGPU device")]
struct Args {
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    pollster::block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    let render_node = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.render_node)
        .with_context(|| format!("open render node {}", args.render_node.display()))?;
    let gbm = GbmDevice::new(render_node).context("create GBM device")?;
    let flags = BufferObjectFlags::RENDERING;
    ensure!(
        gbm.is_format_supported(Format::Argb8888, flags),
        "GBM device does not support ARGB8888 rendering buffers"
    );
    let mut buffer = gbm
        .create_buffer_object::<()>(WIDTH, HEIGHT, Format::Argb8888, flags)
        .context("allocate GBM DMA-BUF")?;
    ensure!(
        buffer.plane_count() == 1,
        "GBM allocation is not single-plane"
    );
    buffer
        .map_mut(0, 0, WIDTH, HEIGHT, |mapping| {
            for pixel in mapping.buffer_mut().chunks_exact_mut(4) {
                pixel.copy_from_slice(&EXPECTED_BGRA);
            }
        })
        .context("fill GBM DMA-BUF")?;

    let metadata = SinglePlaneDmabuf {
        width: WIDTH,
        height: HEIGHT,
        format: wgpu::TextureFormat::Bgra8Unorm,
        view_format: Some(wgpu::TextureFormat::Bgra8UnormSrgb),
        modifier: u64::from(buffer.modifier()),
        stride: u64::from(buffer.stride_for_plane(0)),
        offset: u64::from(buffer.offset(0)),
    };
    let dma_fd = buffer.fd_for_plane(0).context("export GBM DMA-BUF plane")?;

    let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(instance_descriptor);
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        })
        .await
        .context("find Vulkan WGPU adapter")?;
    let feature = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    ensure!(
        adapter.features().contains(feature),
        "Vulkan adapter does not expose DMA-BUF import"
    );
    let adapter_info = adapter.get_info();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("Manifold DMA-BUF import spike"),
            required_features: feature,
            ..Default::default()
        })
        .await
        .context("create Vulkan WGPU device with DMA-BUF import")?;

    let texture = import_single_plane(&device, dma_fd, metadata)?;
    let _srgb_view = texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("DMA-BUF import spike sRGB view"),
        format: metadata.view_format,
        ..Default::default()
    });
    let readback_size = u64::from(BYTES_PER_ROW) * u64::from(HEIGHT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("DMA-BUF import readback"),
        size: readback_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("DMA-BUF import verification"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(BYTES_PER_ROW),
                rows_per_image: Some(HEIGHT),
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    let (mapped_tx, mapped_rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        drop(mapped_tx.send(result));
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .context("wait for DMA-BUF readback")?;
    mapped_rx
        .recv()
        .context("DMA-BUF map callback dropped")?
        .context("map DMA-BUF readback")?;
    let bytes = slice
        .get_mapped_range()
        .context("borrow mapped DMA-BUF readback")?;
    let first_pixel = bytes
        .get(..EXPECTED_BGRA.len())
        .context("readback is shorter than one pixel")?;
    ensure!(
        first_pixel == EXPECTED_BGRA,
        "DMA-BUF pixel mismatch: expected {EXPECTED_BGRA:?}, got {first_pixel:?}"
    );

    println!(
        "dmabuf_import=ok adapter={:?} backend={:?} modifier={} stride={} pixel={:02x?}",
        adapter_info.name, adapter_info.backend, metadata.modifier, metadata.stride, first_pixel
    );
    Ok(())
}
