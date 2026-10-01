//! frameit: a temporary, translucent selection rectangle for Wayland/Hyprland.
//!
//! Run it, drag with the left mouse button, release. The rectangle is a real
//! layer-shell overlay, so screen sharing picks it up. Nothing persists after
//! the button is released: the process exits and the compositor drops the
//! surfaces.

mod config;
mod corners;
mod overlay;

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

use rustix::fs::FlockOperation;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::WlOutput,
    wl_pointer::{self, WlPointer},
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_seat::{self, WlSeat},
    wl_shm::WlShm,
    wl_subcompositor::WlSubcompositor,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use config::{Config, Cursor, Rgba};
use corners::Corners;
use overlay::{Overlay, Rect};

// Linux input event codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const KEY_ESC: u32 = 1;
/// Left and right Meta, Shift, Ctrl and Alt.
const MODIFIER_KEYS: [u32; 8] = [125, 126, 42, 54, 29, 97, 56, 100];

const USAGE: &str = "\
usage: frameit [--config PATH]
       frameit bind [--config PATH]

Drag with the left mouse button to show a temporary selection rectangle.
Release, press Escape, or right-click to exit. Letting go of the modifier
keys that launched frameit before dragging exits as well.

  bind          Register the configured trigger with Hyprland via hyprctl.
  -c, --config  Config file; default $XDG_CONFIG_HOME/frameit/config.toml.

Config keys: fill, border, border_width, border_radius, cursor, trigger.";

/// Compositor objects shared by every overlay.
pub struct Globals {
    pub compositor: WlCompositor,
    pub subcompositor: WlSubcompositor,
    pub layer_shell: ZwlrLayerShellV1,
    pub viewporter: WpViewporter,
    /// Empty input region for the rectangle panes.
    pub empty_region: WlRegion,
    /// 1x1 buffers: fully transparent, translucent fill, border colour.
    pub clear: WlBuffer,
    pub fill: WlBuffer,
    pub border: WlBuffer,
    pub corners: Corners,
    pub border_width: i32,
}

pub struct State {
    pub globals: Globals,
    pub overlays: Vec<Overlay>,
    cursor: Cursor,
    cursor_shapes: Option<WpCursorShapeManagerV1>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    /// Overlay the pointer is currently over.
    hover: Option<usize>,
    /// Latest pointer position, in the hovered overlay's logical coordinates.
    pos: (f64, f64),
    /// `pos` came from a motion event. Enter positions are not trusted:
    /// Hyprland's map-time enter subtracts the monitor position twice
    /// (LayerSurface.cpp, onMap), so they are wrong on secondary outputs.
    pos_known: bool,
    /// The left button is down.
    pressed: bool,
    /// Modifier keys still held from the chord that launched us, re-read from
    /// every keyboard enter and pruned on key release.
    trigger_keys: Vec<u32>,
    /// A modifier chord was seen, so releasing all of it means cancel.
    armed: bool,
    /// Overlay the rectangle is drawn on, and the anchor in global coordinates.
    drag: Option<(usize, (f64, f64))>,
    running: bool,
}

impl State {
    /// Pointer position in global layout coordinates, if it is over an overlay.
    fn global_pos(&self) -> Option<(f64, f64)> {
        self.hover.map(|i| self.overlays[i].to_global(self.pos))
    }

    /// Exit if the modifier chord that launched us is fully released before a
    /// drag started: an accidental trigger leaves nothing behind.
    fn check_disarm(&mut self) {
        if self.armed && self.trigger_keys.is_empty() && !self.pressed {
            self.running = false;
        }
    }

    /// Begin a drag at the pointer. The rectangle is drawn on the output that
    /// contains that point, which is not always the surface the press was
    /// reported against: on map, Hyprland hands pointer focus to the last
    /// overlay mapped wherever the cursor is, and keeps it there until the
    /// mouse moves.
    fn start_drag(&mut self, qh: &QueueHandle<State>) {
        let Some(anchor) = self.global_pos() else {
            return;
        };
        let on = self.overlays.iter().position(|o| o.contains(anchor));
        self.drag = Some((on.or(self.hover).unwrap_or(0), anchor));
        self.update_drag(qh);
    }

    fn update_drag(&mut self, qh: &QueueHandle<State>) {
        let (Some((i, anchor)), Some(cursor)) = (self.drag, self.global_pos()) else {
            return;
        };
        let overlay = &mut self.overlays[i];
        let rect = Rect::from_corners(
            overlay.to_local(anchor),
            overlay.to_local(cursor),
            overlay.size,
        );
        overlay.set_rect(&self.globals, Some(rect), qh);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut bind = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "bind" => bind = true,
            "-c" | "--config" => match args.next() {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => {
                    eprintln!("frameit: --config needs a path\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => {
                eprintln!("frameit: unknown argument '{other}'\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let config = match config_path.or_else(Config::default_path) {
        Some(path) => Config::load(&path),
        None => Config::default(),
    };
    let result = if bind {
        register_bind(&config.trigger)
    } else {
        run(&config)
    };
    if let Err(e) = result {
        eprintln!("frameit: {e}");
        std::process::exit(1);
    }
}

/// Register `trigger` with the running Hyprland so that it launches this
/// binary. Tries the classic config keyword first and falls back to the Lua
/// API, which is what a Lua-configured Hyprland asks for.
fn register_bind(trigger: &str) -> Result<(), Box<dyn std::error::Error>> {
    let keys: Vec<&str> = trigger
        .split('+')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .collect();
    let Some((key, mods)) = keys.split_last() else {
        return Err("trigger is empty".into());
    };
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "frameit".into());

    let hyprctl = |args: &[&str]| -> Result<String, Box<dyn std::error::Error>> {
        let output = Command::new("hyprctl").args(args).output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let classic = format!("{}, {}, exec, {}", mods.join(" "), key, exe);
    let mut reply = hyprctl(&["keyword", "bind", &classic])?;
    if reply.contains("Use eval") {
        let lua = format!(
            "hl.bind(\"{}\", hl.dsp.exec_cmd(\"{}\"))",
            keys.join(" + "),
            exe
        );
        reply = hyprctl(&["eval", &lua])?;
    }
    if reply != "ok" {
        return Err(format!("hyprctl: {reply}").into());
    }
    Ok(())
}

fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // Only one frameit at a time; a second invocation while one is up is a no-op.
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let lock = File::create(
        runtime_dir
            .unwrap_or_else(std::env::temp_dir)
            .join("frameit.lock"),
    )
    .ok();
    if let Some(lock) = &lock
        && rustix::fs::flock(lock, FlockOperation::NonBlockingLockExclusive)
            == Err(rustix::io::Errno::WOULDBLOCK)
    {
        return Ok(());
    }

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let subcompositor: WlSubcompositor = globals.bind(&qh, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=5, ())?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let pixels: WpSinglePixelBufferManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ())?;
    let cursor_shapes: Option<WpCursorShapeManagerV1> = globals.bind(&qh, 1..=2, ()).ok();
    let seat: WlSeat = globals.bind(&qh, 1..=9, ())?;

    let empty_region = compositor.create_region(&qh, ());
    let pixel = |[r, g, b, a]: Rgba| {
        let channel = |v: f64| (v * a * u32::MAX as f64) as u32;
        pixels.create_u32_rgba_buffer(channel(r), channel(g), channel(b), channel(1.0), &qh, ())
    };
    let globals_ = Globals {
        clear: pixel([0.0; 4]),
        fill: pixel(config.fill),
        border: pixel(config.border),
        corners: Corners::new(&shm, config, &qh)?,
        border_width: config.border_width,
        compositor,
        subcompositor,
        layer_shell,
        viewporter,
        empty_region,
    };

    // One overlay per output so the drag can start anywhere.
    let outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| (g.name, g.version.min(4)))
            .collect()
    });
    if outputs.is_empty() {
        return Err("compositor advertises no outputs".into());
    }
    let xdg_outputs: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    let overlays: Vec<Overlay> = outputs
        .into_iter()
        .enumerate()
        .map(|(i, (name, version))| {
            let output = globals
                .registry()
                .bind::<WlOutput, _, _>(name, version, &qh, ());
            // Each output's position in the global layout, so a press that the
            // compositor reports against one overlay can be drawn on another.
            if let Some(manager) = &xdg_outputs {
                manager.get_xdg_output(&output, &qh, i);
            }
            Overlay::new(&globals_, output, &qh)
        })
        .collect();

    let mut state = State {
        globals: globals_,
        overlays,
        cursor: config.cursor,
        cursor_shapes,
        cursor_device: None,
        pointer: None,
        keyboard: None,
        hover: None,
        pos: (0.0, 0.0),
        pos_known: false,
        pressed: false,
        trigger_keys: Vec::new(),
        armed: false,
        drag: None,
        running: true,
    };
    let _ = seat; // kept alive by the compositor; events arrive via Dispatch

    while state.running {
        queue.blocking_dispatch(&mut state)?;
    }
    // Dropping the connection closes the socket; the compositor destroys the
    // overlays immediately. Nothing is left on screen.
    Ok(())
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut State,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            state.cursor_device = state
                .cursor_shapes
                .as_ref()
                .map(|m| m.get_pointer(&pointer, qh, ()));
            state.pointer = Some(pointer);
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut State,
        pointer: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        use wl_pointer::{ButtonState, Event};
        match event {
            Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.hover = state.overlays.iter().position(|o| o.surface == surface);
                state.pos = (surface_x, surface_y);
                state.pos_known = false;
                match (state.cursor, &state.cursor_device) {
                    (Cursor::Hidden, _) => pointer.set_cursor(serial, None, 0, 0),
                    (Cursor::Crosshair, Some(device)) => device.set_shape(serial, Shape::Crosshair),
                    (Cursor::Default, Some(device)) => device.set_shape(serial, Shape::Default),
                    _ => {}
                }
            }
            Event::Leave { .. } => state.hover = None,
            Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pos = (surface_x, surface_y);
                state.pos_known = true;
                if state.pressed && state.drag.is_none() {
                    // The press came before any motion; anchor here instead.
                    state.start_drag(qh);
                }
                state.update_drag(qh);
            }
            Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } => {
                let down = pressed == ButtonState::Pressed;
                match (button, down, state.pressed) {
                    (BTN_LEFT, true, false) => {
                        state.pressed = true;
                        // With an untrusted position the anchor waits for the
                        // first motion event, which lands within a pixel or two.
                        if state.pos_known {
                            state.start_drag(qh);
                        }
                    }
                    (BTN_LEFT, false, true) => state.running = false,
                    (BTN_RIGHT, true, _) => state.running = false,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut State,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use wl_keyboard::{Event, KeyState};
        match event {
            Event::Key {
                key: KEY_ESC,
                state: WEnum::Value(KeyState::Pressed),
                ..
            } => state.running = false,
            Event::Key {
                key,
                state: WEnum::Value(KeyState::Released),
                ..
            } => {
                state.trigger_keys.retain(|&held| held != key);
                state.check_disarm();
            }
            // Every enter lists the keys physically held right now. The
            // modifiers among them are the chord that launched us. (The
            // modifiers mask is not usable for this: Hyprland sends a transient
            // zero mask while moving focus between our overlays.)
            Event::Enter { keys, .. } => {
                state.trigger_keys = keys
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| u32::from_ne_bytes(*bytes))
                    .filter(|key| MODIFIER_KEYS.contains(key))
                    .collect();
                state.armed |= !state.trigger_keys.is_empty();
                state.check_disarm();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut State,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                if let Some(o) = state
                    .overlays
                    .iter_mut()
                    .find(|o| o.is_layer_surface(layer_surface))
                {
                    o.configure(&state.globals, serial, width, height);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.running = false,
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        // Outputs are enumerated once at startup; hotplug during a drag is ignored.
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut State,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let zxdg_output_v1::Event::LogicalPosition { x, y } = event {
            state.overlays[*index].pos = (x, y);
        }
    }
}

// Objects whose events we do not care about (`ignore`) or that have none.
wayland_client::delegate_noop!(State: WlCompositor);
wayland_client::delegate_noop!(State: ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: WlSubcompositor);
wayland_client::delegate_noop!(State: WlRegion);
wayland_client::delegate_noop!(State: ZwlrLayerShellV1);
wayland_client::delegate_noop!(State: WpViewporter);
wayland_client::delegate_noop!(State: WpSinglePixelBufferManagerV1);
wayland_client::delegate_noop!(State: WpCursorShapeManagerV1);
wayland_client::delegate_noop!(State: WpCursorShapeDeviceV1);
wayland_client::delegate_noop!(State: ignore WlBuffer);
wayland_client::delegate_noop!(State: ignore WlOutput);
wayland_client::delegate_noop!(State: ignore WlSurface);
