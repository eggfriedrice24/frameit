//! One transparent fullscreen layer surface per output, plus the subsurfaces
//! that make up the rectangle: three fill bands, four border edges and four
//! corners.
//!
//! Bands and edges are 1x1 single-pixel buffers that the compositor stretches
//! via wp_viewporter; the corners are small textures drawn once at startup
//! (see corners.rs). A redraw is therefore just "set positions and sizes,
//! commit", with no per-frame pixel work.

use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_output::WlOutput, wl_subsurface::WlSubsurface,
    wl_surface::WlSurface,
};
use wayland_client::{Dispatch, QueueHandle};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::Layer,
    zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::corners::OVERSAMPLE;
use crate::{Globals, State};

/// A rectangle in the logical coordinate space of one output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// Normalised rectangle between two corners, clamped to `0..=w` x `0..=h`.
    /// Works for a drag in any direction.
    pub fn from_corners(a: (f64, f64), b: (f64, f64), bounds: (i32, i32)) -> Rect {
        let clamp = |v: f64, max: i32| (v.round() as i32).clamp(0, max);
        let (ax, ay) = (clamp(a.0, bounds.0), clamp(a.1, bounds.1));
        let (bx, by) = (clamp(b.0, bounds.0), clamp(b.1, bounds.1));
        Rect {
            x: ax.min(bx),
            y: ay.min(by),
            w: (ax - bx).abs(),
            h: (ay - by).abs(),
        }
    }
}

/// A subsurface showing a stretched buffer.
struct Pane {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
}

impl Pane {
    fn new(g: &Globals, parent: &WlSurface, qh: &QueueHandle<State>) -> Pane {
        let surface = g.compositor.create_surface(qh, ());
        // Never take pointer focus away from the parent overlay. Double-buffered,
        // so it lands with the pane's first commit.
        surface.set_input_region(Some(&g.empty_region));
        let subsurface = g.subcompositor.get_subsurface(&surface, parent, qh, ());
        let viewport = g.viewporter.get_viewport(&surface, qh, ());
        Pane {
            surface,
            subsurface,
            viewport,
        }
    }

    /// Show `buffer` stretched to `w` x `h` at (`x`, `y`), or hide if empty.
    fn show(&self, buffer: &WlBuffer, x: i32, y: i32, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return self.hide();
        }
        self.subsurface.set_position(x, y);
        self.viewport.set_destination(w, h);
        self.surface.attach(Some(buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }

    /// Show a `size` x `size` window of a corner texture at (`x`, `y`), taken
    /// from offset (`sx`, `sy`) of the texture, all in logical units.
    fn show_corner(&self, buffer: &WlBuffer, x: i32, y: i32, size: i32, sx: i32, sy: i32) {
        let scale = f64::from(OVERSAMPLE);
        let window = f64::from(size) * scale;
        self.viewport
            .set_source(f64::from(sx) * scale, f64::from(sy) * scale, window, window);
        self.show(buffer, x, y, size, size);
    }

    fn hide(&self) {
        self.surface.attach(None, 0, 0);
        self.surface.commit();
    }
}

pub struct Overlay {
    pub output: WlOutput,
    pub surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    viewport: WpViewport,
    /// Logical size from the last configure; (0, 0) until configured.
    pub size: (i32, i32),
    /// Logical position of the output in the global layout (from xdg-output).
    pub pos: (i32, i32),
    /// Top band, middle band, bottom band.
    fill: [Pane; 3],
    /// Top, bottom, left, right.
    edge: [Pane; 4],
    /// Top-left, top-right, bottom-left, bottom-right.
    corner: [Pane; 4],
    /// Rectangle currently committed, or None if hidden.
    shown: Option<Rect>,
    /// Rectangle that should be on screen after the next redraw.
    wanted: Option<Rect>,
    /// Outstanding frame callback; no new commit until it fires.
    frame_callback: Option<WlCallback>,
}

impl Overlay {
    pub fn new(g: &Globals, output: WlOutput, qh: &QueueHandle<State>) -> Overlay {
        let surface = g.compositor.create_surface(qh, ());
        let viewport = g.viewporter.get_viewport(&surface, qh, ());
        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            Layer::Overlay,
            "frameit".into(),
            qh,
            (),
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_size(0, 0);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        // First commit without a buffer asks the compositor for a configure.
        surface.commit();

        // Creation order is stacking order: fill lowest, corners on top.
        let fill = std::array::from_fn(|_| Pane::new(g, &surface, qh));
        let edge = std::array::from_fn(|_| Pane::new(g, &surface, qh));
        let corner = std::array::from_fn(|_| Pane::new(g, &surface, qh));

        Overlay {
            output,
            surface,
            layer_surface,
            viewport,
            size: (0, 0),
            pos: (0, 0),
            fill,
            edge,
            corner,
            shown: None,
            wanted: None,
            frame_callback: None,
        }
    }

    /// Handle a configure: size the transparent parent to the output and map it.
    pub fn configure(&mut self, g: &Globals, serial: u32, width: u32, height: u32) {
        let clamp = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        self.size = (clamp(width), clamp(height));
        self.layer_surface.ack_configure(serial);
        self.viewport
            .set_destination(self.size.0.max(1), self.size.1.max(1));
        self.surface.attach(Some(&g.clear), 0, 0);
        self.surface.damage_buffer(0, 0, 1, 1);
        self.surface.commit();
    }

    /// Request that `rect` (or nothing) be on screen, throttled to the display
    /// refresh via frame callbacks so a 1 kHz mouse does not cause 1 kHz commits.
    pub fn set_rect(&mut self, g: &Globals, rect: Option<Rect>, qh: &QueueHandle<State>) {
        self.wanted = rect;
        if self.frame_callback.is_none() {
            self.redraw(g, qh);
        }
    }

    /// Called when the compositor signals the previous frame was presented.
    pub fn frame_done(&mut self, g: &Globals, qh: &QueueHandle<State>) {
        self.frame_callback = None;
        if self.wanted != self.shown {
            self.redraw(g, qh);
        }
    }

    fn redraw(&mut self, g: &Globals, qh: &QueueHandle<State>) {
        if self.wanted == self.shown {
            return;
        }
        match self.wanted {
            Some(r) if r.w > 0 && r.h > 0 => self.layout(g, r),
            _ => self.panes().for_each(Pane::hide),
        }
        // Subsurfaces are synchronised: committing the parent applies all of
        // the above atomically. Ask for a frame callback first so we know when
        // it is safe to commit again.
        self.frame_callback = Some(self.surface.frame(qh, ()));
        self.surface.commit();
        self.shown = self.wanted;
    }

    /// Place the eleven panes for rectangle `r`.
    fn layout(&self, g: &Globals, r: Rect) {
        let b = g.border_width;
        let c = g.corners.size;
        // Corners shrink for rectangles smaller than two of them, showing only
        // the outer part of their texture.
        let ce = c.min((r.w + 1) / 2).min((r.h + 1) / 2);
        let crop = c - ce;
        let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);

        let corners = [
            (x0, y0, 0, 0),
            (x1 - ce, y0, crop, 0),
            (x0, y1 - ce, 0, crop),
            (x1 - ce, y1 - ce, crop, crop),
        ];
        for (pane, (buffer, (x, y, sx, sy))) in self
            .corner
            .iter()
            .zip(g.corners.buffers.iter().zip(corners))
        {
            pane.show_corner(buffer, x, y, ce, sx, sy);
        }

        let (bw, bh) = (b.min(r.w), b.min(r.h));
        let edges = [
            (x0 + ce, y0, r.w - 2 * ce, bh),
            (x0 + ce, y1 - bh, r.w - 2 * ce, bh),
            (x0, y0 + ce, bw, r.h - 2 * ce),
            (x1 - bw, y0 + ce, bw, r.h - 2 * ce),
        ];
        for (pane, (x, y, w, h)) in self.edge.iter().zip(edges) {
            pane.show(&g.border, x, y, w, h);
        }

        // Between the top corners, the middle, and between the bottom corners.
        let bands = [
            (x0 + ce, y0 + b, r.w - 2 * ce, ce - b),
            (x0 + b, y0 + ce, r.w - 2 * b, r.h - 2 * ce),
            (x0 + ce, y1 - ce, r.w - 2 * ce, ce - b),
        ];
        for (pane, (x, y, w, h)) in self.fill.iter().zip(bands) {
            pane.show(&g.fill, x, y, w, h);
        }
    }

    fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.fill.iter().chain(&self.edge).chain(&self.corner)
    }

    pub fn is_layer_surface(&self, ls: &ZwlrLayerSurfaceV1) -> bool {
        &self.layer_surface == ls
    }

    /// Surface-local to global layout coordinates.
    pub fn to_global(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (x + self.pos.0 as f64, y + self.pos.1 as f64)
    }

    /// Global layout to surface-local coordinates.
    pub fn to_local(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (x - self.pos.0 as f64, y - self.pos.1 as f64)
    }

    /// Whether a global position lies within this overlay.
    pub fn contains(&self, global: (f64, f64)) -> bool {
        let (x, y) = self.to_local(global);
        x >= 0.0 && y >= 0.0 && x <= self.size.0 as f64 && y <= self.size.1 as f64
    }
}

// Nothing to do for the events of these objects.
wayland_client::delegate_noop!(State: ignore WlSubsurface);
wayland_client::delegate_noop!(State: ignore WpViewport);

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut State,
        callback: &WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &wayland_client::Connection,
        qh: &QueueHandle<State>,
    ) {
        // The only callbacks we create are frame callbacks; match it to its overlay.
        let overlay = state
            .overlays
            .iter()
            .position(|o| o.frame_callback.as_ref() == Some(callback));
        if let Some(i) = overlay {
            let g = &state.globals;
            state.overlays[i].frame_done(g, qh);
        }
    }
}
