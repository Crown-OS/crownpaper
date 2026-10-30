# Crownpaper

Crownpaper is a wallpaper daemon for the CrownOS and other Wayland based desktop.

It currently supports:

- Still wallpapers: JPEG, PNG, WebP, BMP
- Video wallpapers with VA-API hardware decoding

## Features

- Beautiful cross-blur transition between wallpapers powered by wgpu (Vulkan)
- the GPU renderer is released shortly after a transition ends to save memory
- Preloading support for instant wallpaper changes
- Video decoding runs in a separate `crownpaper-video` process, so FFmpeg is loaded only while a video plays

## Installation

Will be available on AUR soon(Delayed because [AUR](https://aur.archlinux.org/register) registeration is down).

```sh
cargo build --release
install -Dm755 target/release/crownpaper target/release/crownpaper-video -t ~/.local/bin
```

Both binaries must be installed side by side.

## Usage

Running `crownpaper` without a subcommand starts the daemon (`crownpaper daemon` does the same), optionally with a wallpaper and a default transition length:

```sh
crownpaper [--image <path>] [--duration <seconds>]
```

Without `--image` it uses `wallpaper` from `.config/crownos/appearance.ron`.

```ron
{
    wallpaper: "/home/marvelxcodes/.backgrounds/wallpaper.jpg",
}
```

Control from CLI:

| Command                        | Description                                           |
| ------------------------------ | ----------------------------------------------------- |
| `crownpaper set <path> [-d s]` | Change the wallpaper                                  |
| `crownpaper preload <paths>…`  | Decode wallpapers into the cache for an instant `set` |
| `crownpaper unload <paths>…`   | Drop preloaded wallpapers                             |
| `crownpaper preload-renderer`  | Keep the GPU renderer warm                            |
| `crownpaper unload-renderer`   | Let the renderer be released at rest                  |
| `crownpaper ping`              | Health check                                          |
| `crownpaper quit`              | Stop the daemon                                       |

Set `RUST_LOG` to change log verbosity.

## Dependencies

- A Wayland compositor with `wlr-layer-shell`, `wp_viewporter` and `linux-dmabuf` like [Hyprland](https://hypr.land/), [Sway](https://swaywm.org/), [Niri](https://github.com/niri-wm/niri)
- A Vulkan GPU driver
- libturbojpeg (found through `pkg-config`)
- FFmpeg libraries and VA-API drivers for video wallpapers

(To compile from source)

- Rust 2024 edition toolchain
- Sibling CrownOS crates: `crownos-ipc` and `crownos-config`, expected at `../crownos-ipc` and `../crownos-config`

## Stability

From 1.0 the IPC protocol exposed by the `crownpaper` library crate follows semantic versioning.

## License

MIT, see [LICENSE](LICENSE).
