# Installing niri-zoom

This guide covers building `niri-zoom` and wiring it into **niri**. The tool
isn't distro-specific — the build is a plain `cargo build` — but the keybind,
autostart, and layer-rule snippets below use niri's config syntax.

> **Before you start** — your compositor must support `wlr-layer-shell` and
> `wlr-screencopy`. niri does. See the [README](README.md#compatibility) for
> the full compatibility matrix.

## 1. Prerequisites

- A Linux system with a **Wayland** compositor (niri recommended).
- The Rust toolchain: `rustc` + `cargo`.

    ```sh
    # Arch / CachyOS
    sudo pacman -S rust

    # Debian / Ubuntu
    sudo apt install cargo

    # Fedora
    sudo dnf install cargo
    ```

- A `~/.local/bin` directory **plus `~/.local/bin` on your `$PATH`** for
  interactive shells:

    ```sh
    mkdir -p ~/.local/bin
    ```

    > **niri gotcha:** niri's own `spawn`/`spawn-at-startup` environment does
    > **not** include `~/.local/bin`. Always use an **absolute path**
    > (`/home/you/.local/bin/niri-zoomd`) in niri config, never a bare binary
    > name.

## 2. Build

```sh
cd niri-zoom
cargo build --release
```

This produces two binaries:

- `target/release/niri-zoomd` — the background daemon
- `target/release/niri-zoomctl` — the one-shot CLI (invoked from keybinds)

## 3. Install

```sh
cp target/release/niri-zoomd target/release/niri-zoomctl ~/.local/bin/
```

### Rebuilding while the daemon is running

If the daemon is running, `cp` fails with **"Text file busy"**. Kill it first,
then copy, then relaunch:

```sh
pkill -9 -f "^niri-zoomd$"
cp target/release/niri-zoomd target/release/niri-zoomctl ~/.local/bin/
```

## 4. Wire up niri

### Autostart the daemon

Add to `~/.config/niri/cfg/autostart.kdl`:

```kdl
spawn-at-startup "/home/you/.local/bin/niri-zoomd"
```

### Add keybinds

Add to `~/.config/niri/cfg/keybinds.kdl`:

```kdl
// ─── Zoom (niri-zoom) ───
CTRL+WheelScrollUp   cooldown-ms=0 { spawn "/home/you/.local/bin/niri-zoomctl" "in"; }
CTRL+WheelScrollDown cooldown-ms=0 { spawn "/home/you/.local/bin/niri-zoomctl" "out"; }
CTRL+Mod+Z           { spawn "/home/you/.local/bin/niri-zoomctl" "reset"; }
```

`cooldown-ms=0` is important on the scroll binds — without it niri throttles
the scroll events and zooming feels sluggish.

### Optional: layer rule to hide the overlay from screen capture

Add to `~/.config/niri/cfg/rules.kdl` so other screen-capture consumers
(`grim`, OBS, etc.) don't see the overlay while it's up:

```kdl
layer-rule {
    match namespace="^niri-zoom$"
    block-out-from "screen-capture"
}
```

> This isn't what prevents the tool from photographing its own overlay (the
> daemon destroys and recreates its surface around each capture for that); it
> just keeps *other* screencopy clients from seeing it.

## 5. Usage

After reloading niri config, the keybinds are live:

| Keys | Action |
| --- | --- |
| `Ctrl + Scroll Up` | Zoom in (0.25x steps, up to 6x) |
| `Ctrl + Scroll Down` | Zoom out (auto-closes at 1x) |
| `Ctrl + Super/Mod + Z` | Reset/close zoom |

You can also test the socket directly without a keybind:

```sh
~/.local/bin/niri-zoomctl in      # zoom in one step
~/.local/bin/niri-zoomctl out     # zoom out one step
~/.local/bin/niri-zoomctl reset   # close zoom immediately
```

## 6. Uninstall

```sh
pkill -9 -f "^niri-zoomd$"
rm ~/.local/bin/niri-zoomd ~/.local/bin/niri-zoomctl
```

Then remove the autostart line and keybinds from your niri config.

## Troubleshooting

- **`niri-zoomctl: could not connect to ... (Connection refused). Is niri-zoomd running?`**
  — the daemon isn't running (yet). Check it started: `pgrep -a niri-zoomd`.
  If not, launch it manually to see its log output:
  `~/.local/bin/niri-zoomd`.
- **`cp: ... Text file busy`** — the daemon binary is currently running, see
  [Rebuilding while the daemon is running](#rebuilding-while-the-daemon-is-running).
- **Nothing happens when you scroll** — confirm the daemon is running and that
  the keybind `spawn` paths match the actual install location (niri won't use
  `$PATH`). Test the ctl directly with `~/.local/bin/niri-zoomctl in`.
- **Blurry overlay** — the compositor lacks `wp_viewporter`. The tool still
  works, but on fractional-scale outputs (e.g. 1.5x) it will look softer.
- **Overlay shows a frozen/shrunk image** — you're hitting one of the
  historical coordinate-space or stale-redraw bugs. These are fixed in the
  current code; rebuild from the latest source first.