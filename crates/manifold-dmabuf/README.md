# Manifold DMA-BUF

This crate is the narrow unsafe Linux adapter between registered DMA-BUF frame
resources and the presentation client's WGPU device.

The first supported path is one plane imported by WGPU's Vulkan HAL using
`VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`. The
result is wrapped as an ordinary WGPU texture; WGPU remains the presentation
renderer.

Run the real import proof through Nix:

```sh
nix develop --command cargo run --release -p manifold-dmabuf \
  --bin dmabuf-import-spike
```

The proof allocates an ARGB8888 GBM buffer on the render node, writes a known
pixel pattern, exports its DMA-BUF descriptor, imports it into the selected
Vulkan WGPU device without a CPU image copy, performs a GPU texture-to-buffer
copy, and verifies the resulting bytes.
