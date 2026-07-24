# Manifold DMA-BUF

This crate is the narrow unsafe Linux adapter between registered DMA-BUF frame
resources and the presentation client's WGPU device.

The first supported path is one plane imported by WGPU's Vulkan HAL using
`VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`. The
result is wrapped as an ordinary WGPU texture; WGPU remains the presentation
renderer.

Acceptance belongs to the external presentation-client workloads, which import
real managed-window DMA-BUFs and retain client-composed frames. A synthetic
buffer/readback spike is not evidence for the final presentation path.
