# gpui-pre with video surfaces

A fork of three crates of the [`gpui-pre`](https://crates.io/crates/gpui-pre) 0.3.10 snapshot of
[Zed's GPUI](https://github.com/zed-industries/zed) (zed@`08cbb88`), adding `Window::paint_surface` and `surface()`:

- on **Windows**, for Direct3D 11 textures shared through NT handles (`gpui-pre-windows`);
- on **Linux**, for dma-bufs (`DmaBufTexture`), imported with no copy by GPUI's wgpu renderer (`gpui-pre-wgpu`): on
  Vulkan with `VK_EXT_image_drm_format_modifier`, on Vulkan with LINEAR buffers only (Mesa's lavapipe), or on OpenGL
  through EGL. `dma_buf_import()` tells the producer which render node and modifiers GPUI's device takes.

It exists so that [`player-gpui`](https://github.com/vegidio/player-rs) can show video until the change is in upstream
GPUI and a GPUI-Kit release ships it.

- `gpui-pre/` and `gpui-pre-windows/` are the crates.io 0.3.10 sources. The first commit is them untouched; the second
  is the Windows change.
- `gpui-pre-wgpu/` is the crates.io 0.3.10 source. The third commit is it untouched; the fourth is the Linux change, in
  `gpui-pre` and `gpui-pre-wgpu`.
- Every other `gpui-pre-*` crate is unchanged and comes from crates.io.
- Versions and package names stay `0.3.10`, `gpui-pre`, `gpui-pre-windows` and `gpui-pre-wgpu`, so
  `[patch.crates-io]` replaces them wherever they are pinned (`=0.3.10`), GPUI-Kit 0.8.1 included.

Apps take it through `[patch.crates-io]`; see `player-gpui`'s README for the exact lines.

## Tests

The upstream crates' unit tests need Zed's repository to build, so the Linux change is tested by integration tests in
`gpui-pre-wgpu`, on every adapter wgpu finds:

```sh
cargo test -p gpui-pre-wgpu --test '*'
```

The import test needs `vgem` (`sudo modprobe vgem`, with access to `/dev/dri`) and skips without it. Steer the
adapters with `VK_DRIVER_FILES` (`/dev/null` hides every Vulkan driver). On WSL2, set `LIBGL_ALWAYS_SOFTWARE=1`:
GL is otherwise Mesa's `d3d12`, which advertises dma-buf import but cannot do it.
