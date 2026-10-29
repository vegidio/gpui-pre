# gpui-pre with Windows surfaces

A fork of two crates of the [`gpui-pre`](https://crates.io/crates/gpui-pre) 0.3.10 snapshot of
[Zed's GPUI](https://github.com/zed-industries/zed) (zed@`08cbb88`), adding `Window::paint_surface` and `surface()` on
Windows for Direct3D 11 textures shared through NT handles. It exists so that
[`player-gpui`](https://github.com/vegidio/player-rs) can show video on Windows until the change is in upstream GPUI
and a GPUI-Kit release ships it.

- `gpui-pre/` and `gpui-pre-windows/` are the crates.io 0.3.10 sources. The first commit is them untouched; the second
  is the whole change. Every other `gpui-pre-*` crate is unchanged and comes from crates.io.
- Versions and package names stay `0.3.10`, `gpui-pre` and `gpui-pre-windows`, so `[patch.crates-io]` replaces them
  wherever they are pinned (`=0.3.10`), GPUI-Kit 0.8.1 included.

Apps take it through `[patch.crates-io]`; see `player-gpui`'s README for the exact lines.
