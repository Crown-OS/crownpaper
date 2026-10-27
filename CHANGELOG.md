# Changelog

## [1.0.0] - 2026-10-27

First stable release. The IPC protocol exposed by the `crownpaper` library crate is stable from this version.

### Added

- Still wallpapers (JPEG, PNG, WebP, BMP) and video wallpapers on every output.
- Cross-blur transition between wallpapers, rendered with wgpu on Vulkan.
- Video decoding in the separate `crownpaper-video` helper with VA-API hardware decoding and zero-copy dmabuf presentation.
- `preload` / `unload` for instant wallpaper changes, and `preload-renderer` / `unload-renderer` to control the GPU renderer lifetime.
- Typed client protocol in the library crate; build it without the graphics stack using `default-features = false`.
- MIT license.

### Changed

- The daemon no longer depends on crownshell; it talks to Wayland directly.
- The GPU renderer is released shortly after a transition ends.
- Cold start to first frame: 262 ms → 57 ms.
- Resident memory at rest: 153 MB → 42 MB.
- Peak memory when changing to a 7680×4320 JPEG: 378 MB → 99 MB.
