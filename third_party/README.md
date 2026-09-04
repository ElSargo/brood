# Maintained Graphics Forks

Brood carries two deliberately narrow source forks because the selected GPUI
presentation design needs external Linux DMA-BUF composition that the published
crates do not expose.

| Crate | Version | Upstream revision | License | Source |
| --- | --- | --- | --- | --- |
| `gpui` | 0.2.2 | `69e2130295c2649963eb639fc70b4f2ee8ea1624` | Apache-2.0 | <https://github.com/zed-industries/zed/tree/69e2130295c2649963eb639fc70b4f2ee8ea1624/crates/gpui> |
| `blade-graphics` | 0.7.1 | `88b6c64cd7a32dd933acc4e5266808a07143ce84` | MIT | <https://github.com/kvark/blade/tree/88b6c64cd7a32dd933acc4e5266808a07143ce84/blade-graphics> |

The workspace pins the GPUI version exactly and replaces both crates through
`[patch.crates-io]`. The Nix package source explicitly includes both fork
directories. The audited build uses Rust 1.96.0, Nix 2.34.8, GPUI's Wayland and
X11 features, and Blade's Vulkan renderer. At import time the source trees are
approximately 8 MiB and 3 MiB respectively.

## Patch boundary

The GPUI fork adds a retained `ExternalSurface` scene primitive, a public
surface element, source cropping and Y inversion, and renderer-side import and
lifetime handling. It also retains the current Wayland output refresh rate for
the presentation scheduler and tolerates compositors without a seat, which is
required by the headless nested harness. Small cross-platform lifetime fixes
keep the exact published release compiling on the selected toolchain.

The Blade fork adds Linux DMA-BUF external-memory descriptors, Vulkan external
memory and explicit DRM-format-modifier image creation, descriptor validation,
and submission completion needed before a producer buffer is released.

The adapter-facing API is intentionally small. Changes to the two forks must be
reviewed together, tested with both an imported managed surface and ordinary
GPUI content, and recorded here when the upstream revision changes. Prefer an
upstreamable equivalent when one becomes available; remove local patches only
after the nested zero-copy proof passes against it.
