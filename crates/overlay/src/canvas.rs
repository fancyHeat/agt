//! A tiny CPU rasterizer: antialiased rounded rectangles (and so circles and bars) into a
//! premultiplied-alpha pixel buffer. That's all the overlay draws, so no graphics library.

/// A color with straight (non-premultiplied) components in `0..=1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba(pub f32, pub f32, pub f32, pub f32);

impl Rgba {
    pub fn with_alpha(self, a: f32) -> Self {
        Rgba(self.0, self.1, self.2, a)
    }
}

/// Premultiplied ARGB pixels, one `u32` each (`0xAARRGGBB`; in memory on little-endian that's
/// B, G, R, A: what a Windows 32-bit DIB / `UpdateLayeredWindow` wants). Rows top to bottom.
pub struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width * height],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Resizes and clears to transparent.
    pub fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width * height, 0);
    }

    /// Becomes a copy of `other` (same size and pixels), reusing the allocation.
    pub fn copy_from(&mut self, other: &Canvas) {
        self.width = other.width;
        self.height = other.height;
        self.pixels.clear();
        self.pixels.extend_from_slice(&other.pixels);
    }

    /// Straight (non-premultiplied) RGBA bytes, e.g. for PNG output.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for &p in &self.pixels {
            let a = p >> 24;
            let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
            out.extend_from_slice(&[
                un((p >> 16) & 0xFF),
                un((p >> 8) & 0xFF),
                un(p & 0xFF),
                a as u8,
            ]);
        }
        out
    }

    /// Fills an antialiased rounded rectangle (`x`, `y`, `w`, `h` in pixels; `r` is clamped to
    /// half the shorter side, so `r = f32::MAX` gives a pill or circle).
    pub fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgba) {
        self.round_rect(x, y, w, h, r, f32::INFINITY, color);
    }

    /// Draws a `width`-pixel outline just inside the rounded rectangle's edge.
    #[allow(clippy::too_many_arguments)]
    pub fn stroke_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        width: f32,
        color: Rgba,
    ) {
        self.round_rect(x, y, w, h, r, width, color);
    }

    /// Covers the band of the shape within `band` pixels of its edge (infinite = filled).
    #[allow(clippy::too_many_arguments)]
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, band: f32, color: Rgba) {
        if w <= 0.0 || h <= 0.0 || color.3 <= 0.0 {
            return;
        }
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        // Distance from the center to where the corner arcs' centers are.
        let (ix, iy) = (w / 2.0 - r, h / 2.0 - r);
        let x0 = (x.floor().max(0.0)) as usize;
        let y0 = (y.floor().max(0.0)) as usize;
        let x1 = ((x + w).ceil() as usize).min(self.width);
        let y1 = ((y + h).ceil() as usize).min(self.height);
        let Rgba(cr, cg, cb, ca) = color;
        for py in y0..y1 {
            let qy = ((py as f32 + 0.5 - cy).abs() - iy).max(0.0);
            let row = &mut self.pixels[py * self.width..(py + 1) * self.width];
            for (px, dst) in row.iter_mut().enumerate().take(x1).skip(x0) {
                let qx = ((px as f32 + 0.5 - cx).abs() - ix).max(0.0);
                // Signed distance to the edge (negative inside). Inside, it's only exact
                // within `r` of the edge, which is all that antialiasing and thin bands need.
                let d = if qx > 0.0 || qy > 0.0 {
                    (qx * qx + qy * qy).sqrt() - r
                } else {
                    // In the inner rectangle: distance to the nearer straight edge.
                    ((px as f32 + 0.5 - cx).abs() - w / 2.0)
                        .max((py as f32 + 0.5 - cy).abs() - h / 2.0)
                };
                let cov = (0.5 - d).clamp(0.0, 1.0) * (0.5 + d + band).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let a = ca * cov;
                *dst = blend(*dst, cr * a, cg * a, cb * a, a);
            }
        }
    }
}

/// Source-over of a premultiplied color onto a premultiplied pixel.
fn blend(dst: u32, r: f32, g: f32, b: f32, a: f32) -> u32 {
    let k = 1.0 - a;
    let ch = |shift: u32, src: f32| -> u32 {
        let d = ((dst >> shift) & 0xFF) as f32;
        (src * 255.0 + d * k + 0.5).min(255.0) as u32
    };
    (ch(24, a) << 24) | (ch(16, r) << 16) | (ch(8, g) << 8) | ch(0, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_fill_covers_inside_and_not_outside() {
        let mut c = Canvas::new(10, 10);
        c.fill_round_rect(2.0, 2.0, 6.0, 6.0, 0.0, Rgba(1.0, 0.0, 0.0, 1.0));
        assert_eq!(c.pixels()[5 * 10 + 5], 0xFFFF0000);
        assert_eq!(c.pixels()[0], 0);
        assert_eq!(c.pixels()[9 * 10 + 9], 0);
    }

    #[test]
    fn premultiplied_and_round_trips_to_straight_rgba() {
        let mut c = Canvas::new(4, 4);
        c.fill_round_rect(0.0, 0.0, 4.0, 4.0, 0.0, Rgba(1.0, 1.0, 1.0, 0.5));
        let p = c.pixels()[5];
        assert_eq!(p >> 24, 128);
        assert_eq!((p >> 16) & 0xFF, 128, "premultiplied");
        assert_eq!(&c.to_rgba8()[20..24], &[255, 255, 255, 128]);
    }

    #[test]
    fn rounded_corners_are_antialiased() {
        let mut c = Canvas::new(20, 20);
        c.fill_round_rect(0.0, 0.0, 20.0, 20.0, 10.0, Rgba(0.0, 0.0, 0.0, 1.0));
        assert_eq!(c.pixels()[0] >> 24, 0, "corner is outside the circle");
        assert_eq!(c.pixels()[10 * 20 + 10] >> 24, 255);
        let edge = (0..20).map(|x| c.pixels()[3 * 20 + x] >> 24);
        assert!(
            edge.into_iter().any(|a| a > 0 && a < 255),
            "partial coverage at the edge"
        );
    }

    #[test]
    fn stroke_covers_only_the_edge() {
        let mut c = Canvas::new(20, 20);
        c.stroke_round_rect(0.0, 0.0, 20.0, 20.0, 4.0, 1.0, Rgba(1.0, 1.0, 1.0, 1.0));
        assert_eq!(c.pixels()[10 * 20] >> 24, 255, "left edge");
        assert_eq!(c.pixels()[10 * 20 + 1] >> 24, 0, "just inside");
        assert_eq!(c.pixels()[10 * 20 + 10], 0, "center");
    }

    #[test]
    fn shapes_outside_the_canvas_are_clipped() {
        let mut c = Canvas::new(4, 4);
        c.fill_round_rect(-10.0, -10.0, 100.0, 100.0, 3.0, Rgba(0.0, 1.0, 0.0, 1.0));
        c.fill_round_rect(50.0, 50.0, 5.0, 5.0, 1.0, Rgba(0.0, 1.0, 0.0, 1.0));
        assert!(c.pixels().iter().all(|&p| p == 0xFF00FF00));
    }
}
