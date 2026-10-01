//! The four corners of the rectangle, drawn once at startup into shared
//! memory. Everything else on screen is a stretched single pixel; the corners
//! are the one part that needs real pixels, and they never change during a
//! drag. Each texture holds the border arc with the fill inside it, so the
//! straight edges and fill bands only have to butt up against them.

use std::error::Error;
use std::io::Write;
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::QueueHandle;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};

use crate::State;
use crate::config::{Config, premultiplied};

/// Textures are rendered at this multiple of their logical size and scaled
/// down by the compositor, which smooths the arc on scale-1 outputs and keeps
/// it crisp on scale-2 ones.
pub const OVERSAMPLE: i32 = 2;

pub struct Corners {
    /// Top-left, top-right, bottom-left, bottom-right.
    pub buffers: [WlBuffer; 4],
    /// Logical size of each corner square: the larger of radius and border.
    pub size: i32,
}

impl Corners {
    pub fn new(shm: &WlShm, config: &Config, qh: &QueueHandle<State>) -> Result<Corners, Box<dyn Error>> {
        let size = config.border_radius.max(config.border_width).max(1);
        let px = size * OVERSAMPLE;
        let stride = px * 4;
        let bytes = (stride * px) as usize;

        // Render the top-left corner, then mirror it into the other three.
        let top_left = render_top_left(px, config);
        let mut data = Vec::with_capacity(bytes * 4);
        for corner in 0..4 {
            for y in 0..px {
                for x in 0..px {
                    let sx = if corner & 1 == 1 { px - 1 - x } else { x };
                    let sy = if corner & 2 == 2 { px - 1 - y } else { y };
                    let i = ((sy * px + sx) * 4) as usize;
                    data.extend_from_slice(&top_left[i..i + 4]);
                }
            }
        }

        let mut file = std::fs::File::from(memfd_create("frameit-corners", MemfdFlags::CLOEXEC)?);
        file.write_all(&data)?;
        let pool = shm.create_pool(file.as_fd(), data.len() as i32, qh, ());
        let buffers = std::array::from_fn(|i| {
            let offset = (i * bytes) as i32;
            pool.create_buffer(offset, px, px, stride, wl_shm::Format::Argb8888, qh, ())
        });
        // The buffers keep the storage alive on the compositor side.
        pool.destroy();
        Ok(Corners { buffers, size })
    }
}

/// Premultiplied ARGB8888 pixels of the top-left corner, `px` x `px`.
fn render_top_left(px: i32, config: &Config) -> Vec<u8> {
    let scale = f64::from(OVERSAMPLE);
    let radius = f64::from(config.border_radius) * scale;
    let border = f64::from(config.border_width) * scale;
    let fill = premultiplied(config.fill);
    let edge = premultiplied(config.border);

    let mut data = Vec::with_capacity((px * px * 4) as usize);
    for y in 0..px {
        for x in 0..px {
            let (cx, cy) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let outer = coverage(cx, cy, 0.0, radius);
            let inner = coverage(cx, cy, border, (radius - border).max(0.0));
            let ring = (outer - inner).max(0.0);
            // ARGB8888 little-endian is B, G, R, A in memory.
            for channel in [2, 1, 0, 3] {
                let value = edge[channel] * ring + fill[channel] * inner;
                data.push((value * 255.0).round() as u8);
            }
        }
    }
    data
}

/// Coverage in 0..=1 of the pixel centred at (`x`, `y`) by the shape whose
/// top and left edges sit at `inset` and whose corner is rounded by `radius`.
/// Straight edges land on pixel boundaries, so only the arc is anti-aliased.
fn coverage(x: f64, y: f64, inset: f64, radius: f64) -> f64 {
    if x < inset || y < inset {
        return 0.0;
    }
    let centre = inset + radius;
    if x >= centre || y >= centre {
        return 1.0;
    }
    let distance = ((x - centre).powi(2) + (y - centre).powi(2)).sqrt();
    (radius - distance + 0.5).clamp(0.0, 1.0)
}

wayland_client::delegate_noop!(State: ignore WlShm);
wayland_client::delegate_noop!(State: WlShmPool);

#[cfg(test)]
mod tests {
    use super::*;

    fn config(radius: i32, width: i32) -> Config {
        Config {
            fill: [0.0, 0.0, 1.0, 0.5],
            border: [1.0, 0.0, 0.0, 1.0],
            border_radius: radius,
            border_width: width,
            ..Config::default()
        }
    }

    /// Pixel at (x, y) as [B, G, R, A].
    fn pixel(data: &[u8], px: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * px + x) * 4) as usize;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    #[test]
    fn rounded_corner_has_transparent_tip_border_edge_and_fill_inside() {
        let px = 6 * OVERSAMPLE;
        let data = render_top_left(px, &config(6, 2));
        assert_eq!(pixel(&data, px, 0, 0)[3], 0, "tip outside the arc is transparent");
        // Where the arc meets the straight edge the last pixel is a hair short
        // of fully covered, so accept near-opaque border there.
        for (x, y) in [(px - 1, 0), (0, px - 1)] {
            let [b, _, r, a] = pixel(&data, px, x, y);
            assert!(r >= 250 && a >= 250 && b == 0, "edge at ({x}, {y}) is border: {:?}", [b, r, a]);
        }
        // Premultiplied half-alpha blue: B = 128, A = 128.
        assert_eq!(pixel(&data, px, px - 1, px - 1), [128, 0, 0, 128], "inside is fill");
        // Something on the arc is partially covered.
        let partial = (0..px).any(|i| matches!(pixel(&data, px, i, px - 1 - i)[3], 1..=254));
        assert!(partial, "arc is anti-aliased");
    }

    #[test]
    fn square_corner_is_solid_border() {
        let px = 2 * OVERSAMPLE;
        let data = render_top_left(px, &config(0, 2));
        for y in 0..px {
            for x in 0..px {
                assert_eq!(pixel(&data, px, x, y), [0, 0, 255, 255]);
            }
        }
    }

    #[test]
    #[ignore = "visual aid: cargo test -- --ignored --nocapture"]
    fn dump_corner() {
        let px = 8 * OVERSAMPLE;
        let data = render_top_left(px, &config(8, 3));
        for y in 0..px {
            let row: String = (0..px)
                .map(|x| match pixel(&data, px, x, y) {
                    [_, _, _, 0] => ' ',
                    [_, _, r, a] if r >= 250 && a >= 250 => '#',
                    [128, _, 0, 128] => '.',
                    _ => '+',
                })
                .collect();
            println!("{row}");
        }
    }
}
