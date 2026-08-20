# niri-zoom

A simple screen magnifier for Wayland. Hold `Ctrl` and scroll to zoom around your cursor.

Built for [niri], but it also works with other compositors that support the required Wayland protocols.

## Demo

![niri-zoom demo](assets/demo.gif)

## Features

- `Ctrl` + scroll to zoom in and out
- Zoom from `1x` to `6x` in `0.25x` steps
- Follows the cursor while zoomed
- Captures the screen once and works from the cached frame
- Fullscreen overlay using `wlr-layer-shell`
- Software rendering — no GPU/EGL required
- Optional `Wait` cursor while zoomed

## Keybinds

| Keys | Action |
| --- | --- |
| `Ctrl + Scroll Up` | Zoom in |
| `Ctrl + Scroll Down` | Zoom out |
| `Ctrl + Super/Mod + Z` | Reset and close |

## Requirements

The compositor needs:

| Protocol | Required |
| --- | --- |
| `wl_compositor`, `wl_shm`, `wl_surface`, `wl_output`, `wl_seat`, `wl_pointer` | Yes |
| `zwlr_layer_shell_v1` | Yes |
| `zwlr_screencopy_manager_v1` | Yes |
| `wp_viewporter` | Optional |
| `wp_cursor_shape_manager_v1` | Optional |

Tested with:

- [niri] — recommended
- Hyprland
- Sway
- river

It won't work with compositors that don't provide the required protocols, such as GNOME/Mutter or KWin.

## Install

See [INSTALL.md](INSTALL.md) for the full setup, including niri keybinds and autostart.

For a quick build:

```sh
cargo build --release
cp target/release/niri-zoomd target/release/niri-zoomctl ~/.local/bin/
