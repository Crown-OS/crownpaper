# crownpaper

GPU wallpaper daemon for the CrownOS Wayland desktop. Shows an image or video on every output and animates changes with a cross-blur transition.

- Still wallpapers: JPEG, PNG, WebP, BMP
- Video wallpapers with VA-API hardware decoding and zero-copy dmabuf presentation

## Features

- Cross-blur transition between wallpapers, rendered with wgpu (Vulkan)
- Lean at rest: the GPU renderer is released shortly after a transition ends
- Video decoding runs in a separate `crownpaper-video` process, so FFmpeg is loaded only while a video plays
- Preloading support for instant wallpaper changes
- Control over the CrownOS IPC bus; the `crownpaper` library crate exposes the typed client protocol

## Installation

```sh
cargo build --release
install -Dm755 target/release/crownpaper target/release/crownpaper-video -t ~/.local/bin
```

Both binaries must be installed side by side. To build only the protocol client without the graphics stack, depend on the crate with `default-features = false`.

## Usage

Start the daemon, optionally with a wallpaper and a default transition length:

```sh
crownpaper daemon [--image <path>] [--duration <seconds>]
```

Without `--image` it uses `wallpaper` from `.config/crownos/appearance.ron`.

Control a running daemon:

| Command                        | Description                                           |
| ------------------------------ | ----------------------------------------------------- |
| `crownpaper set <path> [-d s]` | Transition to an image or video                       |
| `crownpaper preload <paths>…`  | Decode wallpapers into the cache for an instant `set` |
| `crownpaper unload <paths>…`   | Drop preloaded wallpapers                             |
| `crownpaper preload-renderer`  | Keep the GPU renderer warm                            |
| `crownpaper unload-renderer`   | Let the renderer be released at rest                  |
| `crownpaper ping`              | Health check                                          |
| `crownpaper quit`              | Stop the daemon                                       |

Set `RUST_LOG` to change log verbosity.

## Dependencies

- A Wayland compositor with `wlr-layer-shell`, `wp_viewporter` and `linux-dmabuf`
- A Vulkan GPU driver
- libturbojpeg (found through `pkg-config`)
- FFmpeg libraries and VA-API drivers for video wallpapers

(To compile from source)

- Rust 2024 edition toolchain
- Sibling CrownOS crates: `crownos-ipc` and `crownos-config`, expected at `../crownos-ipc` and `../crownos-config`
