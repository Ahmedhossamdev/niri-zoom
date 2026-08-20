use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use calloop_wayland_source::WaylandSource;
use memmap2::MmapMut;
use rustix::fs::MemfdFlags;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use niri_zoom::{glyph, socket_path, MAX_ZOOM, MIN_ZOOM, ZOOM_STEP};

const NAMESPACE: &str = "niri-zoom";

struct CapturedImage {
    width: i32,
    height: i32,
    stride: i32,
    bytes: Vec<u8>,
}

struct OutputInfo {
    wl_output: wl_output::WlOutput,
    name: String,
    // None while we've deliberately torn down our own layer-surface to
    // capture a clean shot of the output behind it (see activate_output).
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    // Lets us submit a native-resolution buffer while presenting it at the
    // surface's logical size, so fractional output scaling (this machine
    // reports 1.5) doesn't blur our content the way it would if we rendered
    // straight at the logical (lower) pixel count. None if the compositor
    // doesn't support wp_viewporter.
    viewport: Option<wp_viewport::WpViewport>,
    width: i32,
    height: i32,
    configured: bool,
    focal_x: f64,
    focal_y: f64,
    frame_cb_pending: bool,
    redraw_dirty: bool,
    capture_in_flight: bool,
    cached: Option<CapturedImage>,
}

struct State {
    qh: QueueHandle<State>,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    screencopy_manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    viewporter: Option<wp_viewporter::WpViewporter>,
    cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    cursor_shape_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    // Serial of the most recent wl_pointer Enter we saw on one of our
    // surfaces - cursor-shape-v1's set_shape requires the enter serial that
    // gave us pointer focus.
    last_enter_serial: u32,
    outputs: Vec<OutputInfo>,
    zoom: f32,
    active_output: Option<usize>,
    activating: bool,
    exit: bool,
}

/// Creates the cursor-shape device for our pointer, if the compositor
/// supports wp_cursor_shape_manager_v1 and we have both a pointer and the
/// manager bound. Safe to call repeatedly - it's a no-op once already set.
fn ensure_cursor_shape_device(state: &mut State, qh: &QueueHandle<State>) {
    if state.cursor_shape_device.is_some() {
        return;
    }
    if let (Some(pointer), Some(mgr)) = (&state.pointer, &state.cursor_shape_manager) {
        state.cursor_shape_device = Some(mgr.get_pointer(pointer, qh, ()));
        eprintln!("niri-zoomd: cursor_shape_device created");
    } else {
        eprintln!(
            "niri-zoomd: cursor_shape_device NOT created (pointer={}, manager={})",
            state.pointer.is_some(),
            state.cursor_shape_manager.is_some()
        );
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).expect("failed to init registry");
    let qh = event_queue.handle();

    let compositor: wl_compositor::WlCompositor =
        globals.bind(&qh, 4..=6, ()).expect("compositor missing");
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm missing");
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .expect("compositor does not support wlr-layer-shell");
    let screencopy_manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .expect("compositor does not support wlr-screencopy");
    let viewporter: Option<wp_viewporter::WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    if viewporter.is_none() {
        eprintln!("niri-zoomd: compositor has no wp_viewporter, overlay will be blurrier on fractional-scale outputs");
    }
    let cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1> =
        globals.bind(&qh, 1..=1, ()).ok();
    if cursor_shape_manager.is_none() {
        eprintln!("niri-zoomd: compositor has no wp_cursor_shape_manager_v1, cursor won't change while zoomed");
    }

    let mut state = State {
        qh: qh.clone(),
        compositor,
        shm,
        layer_shell,
        screencopy_manager,
        viewporter,
        cursor_shape_manager,
        cursor_shape_device: None,
        pointer: None,
        last_enter_serial: 0,
        outputs: Vec::new(),
        zoom: MIN_ZOOM,
        active_output: None,
        activating: false,
        exit: false,
    };

    // Bind existing outputs and the seat by walking the global list ourselves.
    for global in globals.contents().clone_list() {
        match global.interface.as_str() {
            "wl_output" => {
                let output: wl_output::WlOutput =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(4), &qh, ());
                add_output(&mut state, output, "unknown".to_string());
            }
            "wl_seat" => {
                let seat: wl_seat::WlSeat =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(7), &qh, ());
                let pointer = seat.get_pointer(&qh, ());
                state.pointer = Some(pointer);
                ensure_cursor_shape_device(&mut state, &qh);
            }
            _ => {}
        }
    }

    event_queue
        .roundtrip(&mut state)
        .expect("initial roundtrip failed");
    event_queue
        .roundtrip(&mut state)
        .expect("second roundtrip failed");

    let socket = setup_socket();

    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().expect("failed to create event loop");
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .expect("failed to insert wayland source");

    socket
        .set_nonblocking(true)
        .expect("failed to set socket nonblocking");
    loop_handle
        .insert_source(
            Generic::new(socket, Interest::READ, Mode::Level),
            |_readiness, listener, state: &mut State| {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => handle_ctl_connection(state, &mut stream),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            eprintln!("niri-zoomd: accept error: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to insert socket source");

    eprintln!(
        "niri-zoomd: ready, listening on {}",
        socket_path().display()
    );

    // If the Wayland connection ever dies (e.g. a protocol error), its fd
    // can stay permanently "ready" and turn a blocking dispatch() into a
    // tight, 100%-CPU busy loop instead of returning an error. Detect that
    // by watching for many consecutive sub-millisecond dispatch() calls and
    // exit cleanly rather than spinning forever.
    let mut fast_spins = 0u32;
    loop {
        let start = std::time::Instant::now();
        event_loop
            .dispatch(None, &mut state)
            .expect("event loop dispatch failed");
        if state.exit {
            break;
        }
        if start.elapsed() < std::time::Duration::from_millis(1) {
            fast_spins += 1;
            if fast_spins > 500 {
                eprintln!("niri-zoomd: event loop is spinning without progress (dead connection?), exiting");
                std::process::exit(1);
            }
        } else {
            fast_spins = 0;
        }
    }
}

fn setup_socket() -> UnixListener {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    UnixListener::bind(&path).expect("failed to bind control socket")
}

fn handle_ctl_connection(state: &mut State, stream: &mut UnixStream) {
    let mut buf = [0u8; 64];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let cmd = String::from_utf8_lossy(&buf[..n]);
    let cmd = cmd.trim();
    match cmd {
        "in" => zoom_in(state),
        "out" => zoom_out(state),
        "reset" => zoom_reset(state),
        _ => eprintln!("niri-zoomd: unknown command {cmd:?}"),
    }
}

fn add_output(state: &mut State, wl_output: wl_output::WlOutput, name: String) {
    state.outputs.push(OutputInfo {
        wl_output,
        name,
        surface: None,
        layer_surface: None,
        viewport: None,
        width: 0,
        height: 0,
        configured: false,
        focal_x: 0.0,
        focal_y: 0.0,
        frame_cb_pending: false,
        redraw_dirty: false,
        capture_in_flight: false,
        cached: None,
    });
    // Layer-surfaces are created lazily, only once a zoom session actually
    // starts (see zoom_in), not here at startup. Every mapped niri-zoom
    // surface matches the `block-out-from "screen-capture"` layer-rule in
    // rules.kdl, and since it's fullscreen, leaving one mapped at idle blacks
    // out the entire output for screen-capture consumers like OBS 100% of
    // the time instead of only while actually zoomed in.
}

/// (Re)creates the layer-surface for an output if it doesn't currently have
/// one. Used both at startup and to re-show the overlay after a capture
/// cycle tore it down (see activate_output).
fn ensure_layer_surface(state: &mut State, idx: usize) {
    if state.outputs[idx].surface.is_some() {
        return;
    }
    let wl_output = state.outputs[idx].wl_output.clone();

    let surface = state.compositor.create_surface(&state.qh, ());
    // Empty (click-through) input region until we activate zoom.
    let region = state.compositor.create_region(&state.qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    let layer_surface = state.layer_shell.get_layer_surface(
        &surface,
        Some(&wl_output),
        zwlr_layer_shell_v1::Layer::Overlay,
        NAMESPACE.to_string(),
        &state.qh,
        idx as u32,
    );
    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    layer_surface.set_size(0, 0);
    let viewport = state
        .viewporter
        .as_ref()
        .map(|vp| vp.get_viewport(&surface, &state.qh, ()));

    surface.commit();

    let o = &mut state.outputs[idx];
    o.surface = Some(surface);
    o.layer_surface = Some(layer_surface);
    o.viewport = viewport;
    o.configured = false;
}

/// Tears down the layer-surface entirely so this output structurally cannot
/// capture its own (already-zoomed) content when we screencopy it.
fn destroy_layer_surface(state: &mut State, idx: usize) {
    let o = &mut state.outputs[idx];
    if let Some(vp) = o.viewport.take() {
        vp.destroy();
    }
    if let Some(ls) = o.layer_surface.take() {
        ls.destroy();
    }
    if let Some(s) = o.surface.take() {
        s.destroy();
    }
    o.configured = false;
    o.frame_cb_pending = false;
    o.redraw_dirty = false;
}

fn zoom_in(state: &mut State) {
    if state.active_output.is_none() && !state.activating {
        state.activating = true;
        // Surfaces are created lazily (see add_output), so bring every
        // output's layer-surface into existence now, before grabbing input
        // on it to find out (via wl_pointer::Enter) which output the cursor
        // is over.
        for idx in 0..state.outputs.len() {
            ensure_layer_surface(state, idx);
        }
        for idx in 0..state.outputs.len() {
            let Some(surface) = &state.outputs[idx].surface else {
                continue;
            };
            // A brand-new surface hasn't been through Configure yet, so its
            // real width/height aren't known - use a generously oversized
            // region instead. Wayland clips input regions to the surface's
            // actual bounds automatically, so this is equivalent to "the
            // whole surface" without needing to wait for Configure first.
            let full = state.compositor.create_region(&state.qh, ());
            full.add(0, 0, 1_000_000, 1_000_000);
            surface.set_input_region(Some(&full));
            full.destroy();
            surface.commit();
        }
    }
    state.zoom = (state.zoom + ZOOM_STEP).min(MAX_ZOOM);
    eprintln!(
        "niri-zoomd: zoom_in -> {} (active_output={:?})",
        state.zoom, state.active_output
    );
    if let Some(idx) = state.active_output {
        request_redraw(state, idx);
    }
}

fn zoom_out(state: &mut State) {
    if state.active_output.is_none() {
        return;
    }
    state.zoom = (state.zoom - ZOOM_STEP).max(MIN_ZOOM);
    eprintln!(
        "niri-zoomd: zoom_out -> {} (active_output={:?})",
        state.zoom, state.active_output
    );
    if state.zoom <= MIN_ZOOM {
        deactivate(state);
    } else if let Some(idx) = state.active_output {
        request_redraw(state, idx);
    }
}

fn zoom_reset(state: &mut State) {
    state.zoom = MIN_ZOOM;
    deactivate(state);
}

fn deactivate(state: &mut State) {
    eprintln!(
        "niri-zoomd: deactivate (was active_output={:?})",
        state.active_output
    );
    state.active_output = None;
    state.activating = false;
    if let Some(dev) = &state.cursor_shape_device {
        dev.set_shape(
            state.last_enter_serial,
            wp_cursor_shape_device_v1::Shape::Default,
        );
    }
    // Fully destroy every output's layer-surface rather than just clearing
    // it to a transparent, click-through buffer: a mapped-but-invisible
    // surface still matches the `block-out-from "screen-capture"` rule and
    // blacks out screen-capture consumers (OBS etc.) even though nothing is
    // visually happening. Also drops the cached frame immediately, as the
    // documented "zoom back to 1x drops the cached frame" behavior promises.
    for i in 0..state.outputs.len() {
        state.outputs[i].cached = None;
        destroy_layer_surface(state, i);
    }
}

fn clear_surface(state: &mut State, idx: usize) {
    let o = &mut state.outputs[idx];
    if o.width == 0 || o.height == 0 || o.surface.is_none() {
        return;
    }
    let (px_w, px_h) = (o.width, o.height);
    let stride = px_w * 4;
    let (buffer, _mmap) =
        match alloc_shm_buffer(state, px_w, px_h, stride, wl_shm::Format::Argb8888) {
            Some(v) => v,
            None => return,
        };
    // mmap already zero-initialized (transparent) by memfd/ftruncate.
    let o = &state.outputs[idx];
    let Some(surface) = &o.surface else { return };
    surface.attach(Some(&buffer), 0, 0);
    surface.set_buffer_scale(1);
    surface.damage_buffer(0, 0, px_w, px_h);
    surface.commit();
}

/// Tears down the active output's overlay surface entirely (so screencopy
/// structurally cannot capture our own rendered content and cause a
/// recursive "zoom of zoom"), then captures the now-surface-less output.
/// Called once, at activation. We deliberately do NOT recapture
/// periodically after that: panning/zoom redraws reuse this single cached
/// frame, which keeps the daemon's steady-state resource use essentially
/// zero and avoids the runaway allocation loop an earlier per-frame-capture
/// design had.
fn activate_output(state: &mut State, idx: usize) {
    if state.outputs[idx].capture_in_flight {
        return;
    }
    state.outputs[idx].capture_in_flight = true;
    destroy_layer_surface(state, idx);
    let output = state.outputs[idx].wl_output.clone();
    state
        .screencopy_manager
        .capture_output(0, &output, &state.qh, idx as u32);
}

/// Frame-paced redraw request: coalesces bursts of zoom/pan updates into at
/// most one redraw per compositor frame, and defers entirely while a capture
/// cycle has us intentionally torn down OR the freshly-recreated surface
/// hasn't been acked by its first Configure yet (attaching a buffer before
/// that is a fatal wlr-layer-shell protocol error - the zwlr_layer_surface_v1
/// Dispatch impl performs the actual first draw once Configure arrives).
fn request_redraw(state: &mut State, idx: usize) {
    if state.outputs[idx].capture_in_flight
        || state.outputs[idx].surface.is_none()
        || !state.outputs[idx].configured
    {
        return;
    }
    if state.outputs[idx].frame_cb_pending {
        state.outputs[idx].redraw_dirty = true;
        return;
    }
    redraw_from_cache(state, idx);
}

/// Recreates the overlay surface torn down by activate_output and restores
/// its full (input-grabbing) region if it's still the active output. This is
/// a brand new layer-surface object, so per wlr-layer-shell protocol we must
/// NOT attach a buffer until its Configure event arrives - the actual first
/// draw happens there (see the zwlr_layer_surface_v1 Dispatch impl), not
/// here. Called once a capture cycle finishes, however it finishes.
fn show_active_surface(state: &mut State, idx: usize) {
    // If the zoom session already ended while the capture that triggered
    // this was in flight, don't recreate a surface at all - there's nothing
    // to show, and a recreated-but-idle surface would just reintroduce the
    // "mapped 100% of the time" screen-capture blackout bug.
    if state.active_output != Some(idx) {
        return;
    }
    ensure_layer_surface(state, idx);
    // Oversized region rather than o.width/o.height: this surface may have
    // just been recreated and not yet received its Configure, in which case
    // those fields would still be stale/zero. Wayland clips to real bounds.
    let full = state.compositor.create_region(&state.qh, ());
    full.add(0, 0, 1_000_000, 1_000_000);
    if let Some(surface) = &state.outputs[idx].surface {
        surface.set_input_region(Some(&full));
    }
    full.destroy();
}

/// Called when a capture cycle ends without producing a usable frame (Failed
/// event, or BufferDone with a bad format / failed allocation). Undoes the
/// half-activated state that `zoom_in` set up: activation may not have had an
/// Enter yet, in which case ALL outputs are still grabbing input through
/// their full input regions. Leaving that in place turns them into invisible
/// clowns that swallow every click, and `activating` staying true forever
/// would also make every later zoom_in a silent no-op. If the failed capture
/// was for the already-active output, full deactivate() is the cleanest reset.
fn capture_aborted(state: &mut State, idx: usize) {
    if state.active_output == Some(idx) {
        deactivate(state);
    } else {
        state.activating = false;
        // Fully tear down rather than leave click-through placeholders
        // mapped - a later zoom_in() recreates whatever it needs from
        // scratch, and nothing should stay mapped (and screen-capture-
        // blocked) once no session is active.
        for i in 0..state.outputs.len() {
            destroy_layer_surface(state, i);
        }
    }
}

fn redraw_from_cache(state: &mut State, idx: usize) {
    let (logical_w, logical_h) = {
        let o = &state.outputs[idx];
        (o.width, o.height)
    };
    if logical_w == 0 || logical_h == 0 || state.outputs[idx].surface.is_none() {
        return;
    }
    let cached = match &state.outputs[idx].cached {
        Some(c) => c,
        None => return,
    };
    eprintln!(
        "niri-zoomd: redraw idx={idx} zoom={} active_output={:?} fx={:.1} fy={:.1}",
        state.zoom, state.active_output, state.outputs[idx].focal_x, state.outputs[idx].focal_y
    );
    let (fx, fy, zoom) = {
        let o = &state.outputs[idx];
        (o.focal_x, o.focal_y, state.zoom)
    };

    let src_w = cached.width;
    let src_h = cached.height;
    let src_stride = cached.stride;
    let src = &cached.bytes;
    // fx/fy come from wl_pointer in LOGICAL coordinates; convert to physical
    // source-pixel space using the captured frame's true physical size
    // (always exact) rather than the legacy integer wl_output.scale event,
    // which can't represent this machine's fractional 1.5 scale and
    // previously caused badly misaligned cropping.
    let dpr_x = src_w as f64 / logical_w as f64;
    let dpr_y = src_h as f64 / logical_h as f64;

    // Render at the captured frame's native (physical) resolution when we
    // have wp_viewporter to present that buffer back down to the surface's
    // logical size - crisp on fractional-scale outputs like this one.
    // Without it, fall back to a logical-resolution buffer, which the
    // compositor's own upscaling will soften but still renders correctly.
    let has_viewport = state.outputs[idx].viewport.is_some();
    let (px_w, px_h) = if has_viewport {
        (src_w, src_h)
    } else {
        (logical_w, logical_h)
    };

    // Crop window, computed as a whole rectangle and clamped to stay fully
    // inside the source image - NOT by clamping each sampled pixel
    // independently. Per-pixel clamping meant that whenever the window
    // (centered on the cursor) would have gone out of bounds, every
    // out-of-bounds sample collapsed onto the same single edge pixel,
    // which could dominate most of the frame and look like a frozen/shrunk
    // image. Whole-window clamping instead just shifts the window to stay
    // in bounds, like a camera being kept on the desktop - and at zoom=1.0
    // the window always exactly equals the full source image, so the
    // origin is forced to (0, 0) regardless of cursor position.
    let win_w = (src_w as f64 / zoom as f64).min(src_w as f64);
    let win_h = (src_h as f64 / zoom as f64).min(src_h as f64);
    let origin_x = (fx * dpr_x - win_w / 2.0).clamp(0.0, (src_w as f64 - win_w).max(0.0));
    let origin_y = (fy * dpr_y - win_h / 2.0).clamp(0.0, (src_h as f64 - win_h).max(0.0));

    let stride = px_w * 4;
    let (buffer, mut mmap) =
        match alloc_shm_buffer(state, px_w, px_h, stride, wl_shm::Format::Argb8888) {
            Some(v) => v,
            None => return,
        };

    {
        let dst = mmap.as_mut();
        for dy in 0..px_h {
            let sy =
                (origin_y + dy as f64 * win_h / px_h as f64).clamp(0.0, (src_h - 1) as f64) as i32;
            let src_row = &src[(sy * src_stride) as usize..];
            let dst_row_off = (dy * stride) as usize;
            for dx in 0..px_w {
                let sx = (origin_x + dx as f64 * win_w / px_w as f64).clamp(0.0, (src_w - 1) as f64)
                    as i32;
                let sp = (sx * 4) as usize;
                let dp = dst_row_off + (dx * 4) as usize;
                dst[dp..dp + 4].copy_from_slice(&src_row[sp..sp + 4]);
            }
        }
        draw_badge(dst, px_w, px_h, stride, zoom);
    }

    let o = &state.outputs[idx];
    let Some(surface) = &o.surface else { return };
    surface.attach(Some(&buffer), 0, 0);
    surface.set_buffer_scale(1);
    if let Some(viewport) = &o.viewport {
        viewport.set_destination(logical_w, logical_h);
    }
    surface.damage_buffer(0, 0, px_w, px_h);
    if !state.outputs[idx].frame_cb_pending {
        state.outputs[idx].frame_cb_pending = true;
        let surface = state.outputs[idx].surface.as_ref().unwrap();
        let cb = surface.frame(&state.qh, idx as u32);
        let _ = cb;
    }
    state.outputs[idx].surface.as_ref().unwrap().commit();
}

fn draw_badge(dst: &mut [u8], px_w: i32, px_h: i32, stride: i32, zoom: f32) {
    let text = format!("{:.2}x", zoom);
    let scale_px = 6i32; // size of each font "pixel" block
    let glyph_w = 3 * scale_px;
    let glyph_h = 5 * scale_px;
    let gap = scale_px;
    let padding = scale_px * 2;

    let text_w = text.chars().count() as i32 * (glyph_w + gap) - gap;
    let badge_w = text_w + padding * 2;
    let badge_h = glyph_h + padding * 2;
    let margin = 24;
    let x0 = px_w - badge_w - margin;
    let y0 = px_h - badge_h - margin;
    if x0 < 0 || y0 < 0 {
        return;
    }

    // Semi-transparent dark background.
    for y in y0..y0 + badge_h {
        let row = (y * stride) as usize;
        for x in x0..x0 + badge_w {
            let p = row + (x * 4) as usize;
            blend(&mut dst[p..p + 4], [0, 0, 0, 170]);
        }
    }

    let mut cursor_x = x0 + padding;
    for c in text.chars() {
        let bits = glyph(c);
        for (row_idx, row_bits) in bits.iter().enumerate() {
            for col in 0..3 {
                if (row_bits >> (2 - col)) & 1 == 1 {
                    let px = cursor_x + col * scale_px;
                    let py = y0 + padding + row_idx as i32 * scale_px;
                    for yy in py..py + scale_px {
                        let row = (yy * stride) as usize;
                        for xx in px..px + scale_px {
                            let p = row + (xx * 4) as usize;
                            blend(&mut dst[p..p + 4], [255, 255, 255, 255]);
                        }
                    }
                }
            }
        }
        cursor_x += glyph_w + gap;
    }
}

fn blend(pixel: &mut [u8], color: [u8; 4]) {
    // pixel is BGRA (premultiplied) in memory for Argb8888 on little-endian.
    let a = color[3] as u32;
    let inv = 255 - a;
    pixel[0] = ((color[2] as u32 * a + pixel[0] as u32 * inv) / 255) as u8;
    pixel[1] = ((color[1] as u32 * a + pixel[1] as u32 * inv) / 255) as u8;
    pixel[2] = ((color[0] as u32 * a + pixel[2] as u32 * inv) / 255) as u8;
    pixel[3] = 255;
}

fn alloc_shm_buffer(
    state: &State,
    width: i32,
    height: i32,
    stride: i32,
    format: wl_shm::Format,
) -> Option<(wl_buffer::WlBuffer, MmapMut)> {
    let size = (stride as i64) * (height as i64);
    if size <= 0 {
        return None;
    }
    let fd = rustix::fs::memfd_create("niri-zoom-buffer", MemfdFlags::CLOEXEC).ok()?;
    rustix::fs::ftruncate(&fd, size as u64).ok()?;
    let mmap = unsafe { MmapMut::map_mut(&fd).ok()? };
    let pool = state
        .shm
        .create_pool(fd.as_fd(), size as i32, &state.qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, format, &state.qh, ());
    pool.destroy();
    Some((buffer, mmap))
}

// ---- Dispatch impls ----

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_output" {
                let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                add_output(state, output, "unknown".to_string());
            } else if interface == "wl_seat" && state.pointer.is_none() {
                let seat: wl_seat::WlSeat = registry.bind(name, version.min(7), qh, ());
                state.pointer = Some(seat.get_pointer(qh, ()));
                ensure_cursor_shape_device(state, qh);
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_region::WlRegion,
        _: wayland_client::protocol::wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            let idx = *data as usize;
            let dirty = if let Some(o) = state.outputs.get_mut(idx) {
                o.frame_cb_pending = false;
                std::mem::take(&mut o.redraw_dirty)
            } else {
                false
            };
            // Only flush a deferred redraw if this output is still the
            // active zoom target - otherwise a redraw queued just before
            // deactivate() (e.g. reaching 1x) fires afterward and re-draws
            // the stale zoomed frame over the surface deactivate() just
            // cleared, which looked like the overlay getting stuck.
            if dirty && state.active_output == Some(idx) {
                redraw_from_cache(state, idx);
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idx = state.outputs.iter().position(|o| &o.wl_output == output);
        let idx = match idx {
            Some(i) => i,
            None => return,
        };
        if let wl_output::Event::Name { name } = event {
            state.outputs[idx].name = name;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.last_enter_serial = serial;
                let idx = state
                    .outputs
                    .iter()
                    .position(|o| o.surface.as_ref() == Some(&surface));
                let idx = match idx {
                    Some(i) => i,
                    None => return,
                };
                state.outputs[idx].focal_x = surface_x;
                state.outputs[idx].focal_y = surface_y;
                if state.activating {
                    state.activating = false;
                    state.active_output = Some(idx);
                    // Fully tear down the other outputs' surfaces (created
                    // during the input-grab-for-detection phase in zoom_in)
                    // rather than just emptying their input region - a
                    // mapped-but-click-through surface still blocks screen
                    // capture on that output for no benefit once we know
                    // it's not the one being zoomed.
                    let output_count = state.outputs.len();
                    for i in 0..output_count {
                        if i != idx {
                            destroy_layer_surface(state, i);
                        }
                    }
                    activate_output(state, idx);
                }
                // Re-applied on every Enter for the active output, not just
                // the first one: activate_output() destroys and recreates
                // this surface right after the initial Enter (to capture a
                // clean screencopy shot), and the recreated surface getting
                // its full input region committed triggers a SECOND Enter
                // with a fresh serial once the capture completes. That
                // second Enter's surface is the one that actually persists
                // for the rest of the zoom session, so the Wait shape has to
                // be (re-)set here too, or it silently reverts to the
                // default arrow the moment the first surface is destroyed.
                if state.active_output == Some(idx) {
                    if let Some(dev) = &state.cursor_shape_device {
                        dev.set_shape(serial, wp_cursor_shape_device_v1::Shape::Wait);
                    }
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(idx) = state.active_output {
                    state.outputs[idx].focal_x = surface_x;
                    state.outputs[idx].focal_y = surface_y;
                    if state.outputs[idx].cached.is_some() {
                        request_redraw(state, idx);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        data: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer_surface.ack_configure(serial);
            let idx = *data as usize;
            if let Some(o) = state.outputs.get_mut(idx) {
                o.width = width as i32;
                o.height = height as i32;
                o.configured = true;
            }
            if state.active_output == Some(idx) && state.outputs[idx].cached.is_some() {
                request_redraw(state, idx);
            } else {
                clear_surface(state, idx);
            }
        }
    }
}

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_viewporter::WpViewporter,
        _: wp_viewporter::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_viewport::WpViewport, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_viewport::WpViewport,
        _: wp_viewport::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        _: wp_cursor_shape_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
        _: wp_cursor_shape_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

struct PendingCapture {
    format: Option<wl_shm::Format>,
    width: i32,
    height: i32,
    stride: i32,
}

thread_local! {
    static PENDING: std::cell::RefCell<std::collections::HashMap<u32, PendingCapture>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, u32> for State {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        data: &u32,
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let idx = *data;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let fmt = match format {
                    WEnum::Value(f) => Some(f),
                    WEnum::Unknown(_) => None,
                };
                PENDING.with(|p| {
                    p.borrow_mut().insert(
                        idx,
                        PendingCapture {
                            format: fmt,
                            width: width as i32,
                            height: height as i32,
                            stride: stride as i32,
                        },
                    );
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                let pending = PENDING.with(|p| p.borrow_mut().remove(&idx));
                if let Some(p) = pending {
                    if let Some(format) = p.format {
                        if let Some((buffer, mmap)) =
                            alloc_shm_buffer(state, p.width, p.height, p.stride, format)
                        {
                            CAPTURE_BUFFERS.with(|c| {
                                c.borrow_mut()
                                    .insert(idx, (mmap, p.width, p.height, p.stride))
                            });
                            frame.copy(&buffer);
                            return;
                        }
                    }
                    eprintln!(
                        "niri-zoomd: capture output {idx}: bad format or failed to allocate buffer"
                    );
                }
                // Fallthrough from a failed/cancelled BufferDone: destroy the
                // frame so the capture cycle actually ends, release any input
                // grab we set while waiting for activation, and let the next
                // zoom-in retry.
                frame.destroy();
                if let Some(o) = state.outputs.get_mut(idx as usize) {
                    o.capture_in_flight = false;
                }
                capture_aborted(state, idx as usize);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                let data = CAPTURE_BUFFERS.with(|c| c.borrow_mut().remove(&idx));
                if let Some((mmap, width, height, stride)) = data {
                    if let Some(o) = state.outputs.get_mut(idx as usize) {
                        // Only keep the frame if this output is still the
                        // active zoom target. If the user deactivated while
                        // the capture was in flight, drop it instead - it's
                        // stale and would otherwise leak another full-frame
                        // buffer alongside the cached frames deactivate()
                        // already dropped.
                        if state.active_output == Some(idx as usize) {
                            o.cached = Some(CapturedImage {
                                width,
                                height,
                                stride,
                                bytes: mmap.to_vec(),
                            });
                        }
                        o.capture_in_flight = false;
                    }
                }
                frame.destroy();
                show_active_surface(state, idx as usize);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                eprintln!("niri-zoomd: capture Failed for output {idx}");
                frame.destroy();
                if let Some(o) = state.outputs.get_mut(idx as usize) {
                    o.capture_in_flight = false;
                }
                capture_aborted(state, idx as usize);
            }
            _ => {}
        }
    }
}

thread_local! {
    static CAPTURE_BUFFERS: std::cell::RefCell<std::collections::HashMap<u32, (MmapMut, i32, i32, i32)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}
