# niri-zoom

A small external Ctrl+scroll magnifier/zoom tool for **niri** (Wayland, CachyOS).
Two binaries: `niri-zoomd` (background daemon that owns all Wayland state and
does the actual rendering) and `niri-zoomctl` (a one-shot CLI that sends a
single command to the daemon over a Unix socket, meant to be invoked from
niri keybinds).

This is a plain Wayland client — it does **not** patch or rebuild niri/the
compositor. It works entirely through stable/standard Wayland protocols plus
niri's existing `spawn`-on-keybind feature. That was a deliberate choice
after evaluating (and rejecting) a compositor-side implementation as too much
work for what should be a simple tool.

## How it works, end to end

1. niri keybinds (`~/.config/niri/cfg/keybinds.kdl`) map `Ctrl+WheelScrollUp` /
   `Ctrl+WheelScrollDown` / `Ctrl+Mod+Z` to `spawn`-ing `niri-zoomctl in|out|reset`.
2. `niri-zoomctl` connects to `$XDG_RUNTIME_DIR/niri-zoomd.sock` and writes a
   single line (`in`, `out`, or `reset`), then exits immediately.
3. `niri-zoomd` (already running, started via `spawn-at-startup`) reads that
   command and adjusts an in-memory `zoom` level (`MIN_ZOOM..MAX_ZOOM`,
   step `ZOOM_STEP` — see `src/lib.rs`).
4. On the **first** zoom-in from an inactive state, the daemon grabs full
   pointer input on every output's overlay surface and waits for a
   `wl_pointer::Enter` event to find out which output the cursor is
   currently over (and, from `surface_x/y`, the focal point to zoom around).
5. Once it knows the active output, it captures that output's contents
   **once** via `wlr-screencopy` (see "Capture model" below), then paints
   a cropped/scaled view of that single captured frame into a fullscreen
   `wlr-layer-shell` overlay surface (Overlay layer, all edges anchored,
   `keyboard-interactivity: none`).
6. Mouse motion while zoomed re-centers the crop window on the cursor and
   redraws from the *same cached frame* (no new screencopy call).
7. Zooming back down to `MIN_ZOOM` (or hitting reset) tears the overlay
   content down, releases pointer input (empty input region on all
   outputs), and drops the cached frame.

## Why capture-once instead of continuous screencopy

The very first working version re-captured the output on every compositor
frame (up to the display's refresh rate). Each capture allocates a fresh shm
buffer at full output resolution — this pinned a CPU core and grew memory
fast enough to visibly freeze and lag the laptop within seconds (confirmed
live, this was a real incident, not a hypothetical). The fix was an
architectural change, not a throttle: capture **exactly once per
activation**, cache the raw pixels, and do all subsequent pan/zoom-level
changes as pure CPU-side crop+resample from that single cached buffer. This
also means the daemon uses ~0% CPU and steady-state memory at idle.

To make the one-shot capture not accidentally photograph *our own* overlay
(which would produce a "zoom of zoom" recursive mess), the layer-surface is
fully **destroyed** immediately before calling `capture_output`, and
recreated once the capture completes (`destroy_layer_surface` /
`ensure_layer_surface` in `niri-zoomd.rs`). This is more reliable than
relying on the `block-out-from "screen-capture"` layer-rule alone, since that
rule is specific to wlroots' screencopy consumers and isn't something to
build the core correctness of the tool around.

## Wayland protocols used

- `wl_compositor` / `wl_shm` / `wl_surface` / `wl_output` / `wl_seat` /
  `wl_pointer` — core protocol.
- `zwlr_layer_shell_v1` — the fullscreen overlay surface.
- `zwlr_screencopy_manager_v1` — one-shot output capture.
- `wp_viewporter` (stable) — lets the overlay submit a buffer at the
  captured **physical** resolution while presenting it at the surface's
  **logical** size, so fractional output scaling (this machine reports
  `1.5`) doesn't blur the content the way rendering directly at logical
  resolution and letting the compositor upscale it would.
- `wp_cursor_shape_manager_v1` (staging) — sets the pointer to the themed
  `Wait` shape while the overlay owns input, so it's visually obvious the
  screen is in "zoomed/grabbed" mode; resets to `Default` on deactivate.
  All uses of this protocol are optional (`.ok()` on bind) — the tool works
  without it, just without the cursor change.

No GPU/EGL involved — cropping/scaling is done with plain nearest-neighbor
sampling in software over the shm-mapped bytes, including a hand-rolled 3x5
bitmap font (`glyph()` in `src/lib.rs`) for the on-screen "X.XXx" zoom badge.

## Coordinate spaces — the trap to remember

`wl_pointer` reports **logical** (scaled) surface coordinates, but
`wlr-screencopy` always reports the output's true **physical** pixel
dimensions. Never trust the legacy integer `wl_output::Event::Scale` to
convert between them — it can't represent this machine's fractional `1.5`
scale and previously caused a bad coordinate-space bug (looked like the
image was frozen/shrunk, since almost every sampled pixel landed on a
clamped edge). Instead compute `dpr_x/dpr_y = captured_physical_size /
surface_logical_size` from the screencopy-reported dimensions (always
exact) and only use it to convert pointer-derived focal coordinates into
physical source-pixel space.

Also: crop-window clamping must clamp the **whole window's origin** as a
rectangle, not clamp each sampled pixel independently — per-pixel clamping
made out-of-bounds regions collapse onto a single repeated edge pixel,
which could dominate the frame and look like a frozen/shrunk image whenever
the focal point wasn't near center.

## Protocol gotcha: buffer attach before Configure

A freshly created `zwlr_layer_surface_v1` **must** have its first
`Configure` event acked before the client attaches any buffer to the paired
`wl_surface`. Attaching earlier is a fatal protocol error that kills the
whole Wayland connection outright (not recoverable). Since this daemon
destroys and recreates the layer-surface around every capture, this is a
real, recurring risk, not a one-time init concern — `request_redraw` checks
`o.configured` before ever attaching, and the actual first draw after
recreation happens from inside the `Configure` event handler.

There's also a defense-in-depth "spin guard" in `main()`: if the Wayland
connection ever does die from a protocol error, calloop's `dispatch()` can
turn into a 100%-CPU busy loop (dead fd staying "ready" forever) instead of
returning a clean error. The main loop counts consecutive sub-millisecond
dispatch calls and exits after 500 in a row rather than spinning forever.

## Frame pacing

`request_redraw` coalesces bursts of pan/zoom input into at most one redraw
per actual compositor frame: if a `wl_callback` from a previous redraw is
still pending, it just sets a `redraw_dirty` flag instead of drawing again;
the `wl_callback::Done` handler flushes at most one deferred redraw per
frame. **Important**: that deferred flush must check `state.active_output
== Some(idx)` before redrawing — a redraw queued right before zoom hit
`MIN_ZOOM` and deactivated used to fire *after* deactivation and re-paint
the stale zoomed frame back over the (just-cleared) screen, which looked
like the overlay getting permanently stuck showing a frozen image with no
input working. Fixed in commit `b9b8025` — if you ever touch the coalescing
logic again, re-verify this specifically.

## Known limitation / accepted tradeoff

While the overlay is active (zoom > 1x), it owns **all** pointer input on
every output (needed so it can track the cursor for panning) — clicks don't
reach real windows underneath until you zoom back out to 1x. This is a
deliberate, disclosed tradeoff of the current architecture, consistent with
how other external magnifier tools behave (e.g. Windows Magnifier lens
mode), and zooming back to 1x now reliably and instantly hands input back
(see the frame-pacing fix above).

**Planned future work (not yet started):** real click-passthrough while
still zoomed in — clicks would need to be handled by briefly hiding the
overlay, translating the click's zoomed-screen position back to real screen
coordinates, and injecting a synthetic click via a virtual-pointer protocol
(e.g. `zwlr_virtual_pointer_manager_v1`) before reshowing the overlay. This
is a meaningfully larger and riskier change than anything implemented so
far (new protocol, more edge cases, more surface area for the class of
freeze/protocol-error bugs already hit and fixed during initial
development) — deliberately deferred as a separate task.

## Files

- `src/bin/niri-zoomd.rs` — the daemon; almost all of the actual logic.
- `src/bin/niri-zoomctl.rs` — trivial one-shot CLI, just writes a command
  line to the daemon's socket and exits.
- `src/lib.rs` — shared constants (`ZOOM_STEP`, `MIN_ZOOM`, `MAX_ZOOM`),
  `socket_path()`, and the bitmap font used for the on-screen zoom badge.

## Build / install

```sh
cargo build --release
cp target/release/niri-zoomd target/release/niri-zoomctl ~/.local/bin/
```

`~/.local/bin` must already be on `$PATH` for an interactive shell, but
**niri's own `spawn`/`spawn-at-startup` environment does not include it** —
always use absolute paths (`/home/ahmed/.local/bin/niri-zoomd`) in niri
config, never bare binary names.

If you rebuild while the daemon is running, `cp` will fail with "Text file
busy" — kill the running daemon first (`pkill -9 -f "^niri-zoomd$"`), then
copy, then relaunch.

## niri config wiring

- `~/.config/niri/cfg/autostart.kdl` — `spawn-at-startup` for `niri-zoomd`.
- `~/.config/niri/cfg/keybinds.kdl` — the three `Ctrl+WheelScroll*`/`Ctrl+Mod+Z`
  binds under the "Zoom (niri-zoom)" section, `spawn`-ing `niri-zoomctl`.
- `~/.config/niri/cfg/rules.kdl` — deliberately has **no** `block-out-from`
  layer-rule for the `^niri-zoom$` namespace. An earlier version did add
  one (`block-out-from "screen-capture"`), intended to hide the overlay
  from screenshot/capture tools, but that's not what prevents the
  self-capture recursion bug (that's the destroy/recreate dance above) —
  and it also hid the overlay from OBS recordings, which isn't wanted:
  the user records zoom sessions and wants OBS to show the zoomed view.
  Removed for that reason (see bug history #8 below).

## Shortcuts

- `Ctrl + Scroll Up` — zoom in
- `Ctrl + Scroll Down` — zoom out (auto-closes once back at 1x)
- `Ctrl + Super/Mod + Z` — reset/close zoom immediately

## Debug logging

`niri-zoomd.rs` currently has `eprintln!` diagnostic logging left in on
purpose in `redraw_from_cache`, `zoom_in`, `zoom_out`, and `deactivate` —
added while chasing the freeze/coordinate/stale-redraw bugs above and kept
for now since it's cheap and has been useful for correlating live-tested
behavior against actual state transitions. Safe to strip once the tool has
been stable through a few real sessions; grep for `eprintln!("niri-zoomd:`
to find all of it.

## Verified-fixed bug history (chronological)

1. **Freeze/high memory** — unthrottled per-frame recapture loop, ~48MB
   fresh allocations per frame at up to display refresh rate. Fixed by the
   capture-once-per-activation architecture described above.
2. **Fatal protocol error / CPU spin** — buffer attached before the
   recreated layer-surface's first `Configure` was acked. Fixed by gating
   `request_redraw` on `o.configured`; spin-guard added as a backstop.
3. **Fractional-scale coordinate bug** — used the legacy integer
   `wl_output::Event::Scale` (can't represent `1.5`) instead of deriving
   `dpr_x/dpr_y` from screencopy's true physical dimensions. Looked like a
   frozen/shrunk static image.
4. **Per-pixel clamping artifact** — switched to whole-crop-window origin
   clamping so `zoom == 1.0` always exactly equals the full source image
   regardless of cursor position.
5. **Blurry overlay** — was rendering at logical resolution and letting the
   compositor's fractional-scale upscale soften it. Fixed with
   `wp_viewporter`: render at native/physical resolution, present at
   logical size via `viewport.set_destination`.
6. **Stuck-at-1x / frozen image after zoom-out** — a redraw deferred behind
   a pending frame callback fired *after* `deactivate()` had already run,
   re-painting stale zoomed content over the just-cleared screen. Fixed by
   checking `state.active_output == Some(idx)` before flushing a deferred
   redraw (commit `b9b8025`).
7. **Wait cursor reverting to default arrow mid-session** — the
   `wp_cursor_shape_device_v1::set_shape(Wait)` call only fired on a
   surface's *first* `wl_pointer::Enter`, but `activate_output()` destroys
   and recreates the layer-surface right after that Enter (to take a clean
   screencopy shot), which triggers a second Enter whose surface is the one
   that actually persists for the rest of the session. Fixed by re-applying
   the shape on every Enter for the active output, not just the first
   (commit `323cdce`).
8. **OBS/screen-capture permanently black** — every output's `niri-zoom`
   layer-surface was created once at startup (`add_output`) and never fully
   destroyed while idle, only cleared to a transparent, click-through
   buffer. Since it's fullscreen and matches the `block-out-from
   "screen-capture"` rule in `rules.kdl` by namespace regardless of its
   content, screen-capture consumers like OBS saw the entire output blacked
   out 100% of the time, not just while actually zoomed in (cursor still
   showed because the portal composites it separately from surface
   capture). Fixed the always-mapped-while-idle part by creating
   layer-surfaces lazily, only when a zoom session actually starts
   (`zoom_in`), and fully destroying them (not just clearing/hiding) on
   `deactivate`, `capture_aborted`, and once the active output is
   determined (the other outputs' detection-phase surfaces). Separately,
   the user wants OBS to actually *show* the zoomed view while recording
   (not be blocked during an active zoom session either), so the
   `block-out-from "screen-capture"` rule itself was removed from
   `rules.kdl` entirely rather than kept for that window — see "niri
   config wiring" above. Tradeoff: a `Print`-key screenshot taken mid-zoom
   will now also include the overlay, since niri has no way to distinguish
   OBS from screenshot tools within the same capture category.
