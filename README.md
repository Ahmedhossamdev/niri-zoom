# niri-zoom

> A lightweight magnifier for Wayland compositors. Hold `Ctrl`, scroll, and zoom
> the screen around your cursor — no compositor patching, no GPU, no GPU/EGL
> rendering.

`niri-zoom` is an external `Ctrl`+scroll magnifier for [niri] and other
compatible Wayland compositors. It runs entirely as a normal, unprivileged
Wayland client using stable/standard protocols plus niri's `spawn`-on-keybind
feature.

It does **not** patch, rebuild, or embed itself in the compositor.

## Demo

<video src="https://github.com/Ahmedhossamdev/niri-zoom/raw/master/assets/nirizoom.mp4" controls></video>

## Who is this for?

- **Anyone on Linux running a Wayland compositor that supports
  `wlr-layer-shell` and `wlr-screencopy` protocols** (see
  [Compatibility](#compatibility)).
- **Not** X11 users — it is a Wayland-only tool and will not run under Xorg.
- **Not** distro-specific — it's a self-contained Rust program. The author
  develops and tests it on **niri** on **CachyOS (Arch)**, but nothing in the
  code is Arch- or CachyOS-specific; it builds and runs on any Linux distro
  with a Rust toolchain.
- **Designed and tested primarily for niri**, where the keybind wiring and the
  `block-out-from "screen-capture"` layer rule are covered.

## Features

- **Ctrl + scroll zoom** — hold `Ctrl` and scroll up to zoom in, scroll down
  to zoom out, in `0.25x` steps from `1x` to `6x`.
- **Cursor-following pan** — the zoom window stays centered on your pointer as
  you move it while zoomed.
- **Capture-once architecture** — the target output is captured a single time
  per activation; every subsequent pan/zoom is a pure CPU-side crop+resample
  from that one cached frame. Idle CPU usage is ~0% and memory is steady.
- **Fullscreen overlay** — zoom happens through a fullscreen
  `wlr-layer-shell` overlay surface on the active output.
- **Optional wait cursor** — while the overlay owns input, the pointer changes
  to a `Wait` cursor so it's obvious the screen is zoomed (only if the
  compositor supports `wp_cursor_shape_manager_v1`; the tool works fine
  without it).
- **No GPU/EGL** — crop/scale uses hand-rolled nearest-neighbor sampling in
  software over plain shm buffers.

## Keyboard shortcuts

| Keys (niri-defaults) | Action |
| --- | --- |
| `Ctrl + Scroll Up` | Zoom in |
| `Ctrl + Scroll Down` | Zoom out (closes automatically back at `1x`) |
| `Ctrl + Super/Mod + Z` | Reset/close zoom immediately |

## Compatibility

The daemon is a plain Wayland client and needs a compositor that exposes:

| Protocol | Purpose | Required |
| --- | --- | --- |
| `wl_compositor`, `wl_shm`, `wl_surface`, `wl_output`, `wl_seat`, `wl_pointer` | Core protocol | ✅ |
| `zwlr_layer_shell_v1` | Fullscreen overlay surface | ✅ |
| `zwlr_screencopy_manager_v1` | One-shot output capture | ✅ |
| `wp_viewporter` | Native-resolution overlay on fractional-scale outputs | ⚠️ optional (blurrier without it) |
| `wp_cursor_shape_manager_v1` | `Wait` cursor while zoomed | ⚠️ optional |

This means it works on **niri** (recommended and tested), **Hyprland**, and
**wlroots-based** compositors like **Sway** and **river**. It will **not** work
on compositors that lack those protocols (e.g. Mutter/GNOME or KWin).

> Note: the `block-out-from "screen-capture"` layer rule shown in the install
> guide is niri-specific. On other compositors you can simply skip it — the
> tool keeps its own copy of the captured frame.

## Install

Full build + wiring guide (including niri config snippets):

- **[INSTALL.md](INSTALL.md)** — build, install to `~/.local/bin`, and wire up
  niri keybinds, autostart, and layer rules.

Quick start (niri + Arch-like distros):

```sh
cargo build --release
cp target/release/niri-zoomd target/release/niri-zoomctl ~/.local/bin/
```

Then add the [keybinds], [autostart], and [layer rule] snippets from
`INSTALL.md` to your niri config.

## How it works (short version)

1. A niri keybind `spawn`s `niri-zoomctl in|out|reset`.
2. `niri-zoomctl` writes a single command line to a Unix socket and exits.
3. The `niri-zoomd` daemon adjusts its in-memory zoom level.
4. On first zoom-in, the daemon grabs pointer input, captures the active output
   **once** (via `wlr-screencopy`), and draws a cropped/scaled view of that
   single frame into a fullscreen overlay surface.
5. Mouse motion redraws from the *cached* frame — no new capture.
6. Zooming back to `1x` (or reset) tears the overlay down and releases input.

See `CLAUDE.md` for the full architectural write-up including the capture-once
design rationale, coordinate-space handling, and the verified-fixed bug
history.

## Project layout

```
src/
  lib.rs            shared constants (ZOOM_STEP, MIN_ZOOM, MAX_ZOOM),
                    socket path, bitmap font
  bin/
    niri-zoomd.rs   background daemon — owns all Wayland state & rendering
    niri-zoomctl.rs one-shot CLI — sends a command to the daemon and exits
```

## Development

`niri-zoomd` leaves `eprintln!` diagnostics in `redraw_from_cache`,
`zoom_in`, `zoom_out`, and `deactivate` — they're cheap and useful when
correlating live-tested behavior with state transitions. Strip them (grep for
`eprintln!("niri-zoomd:`) once the tool has been stable through real sessions.

## License

[MIT](LICENSE)

[niri]: https://github.com/YaLTeR/niri
[keybinds]: INSTALL.md#wire-up-niri
[autostart]: INSTALL.md#autostart-the-daemon
[layer rule]: INSTALL.md#optional-layer-rule-to-hide-the-overlay