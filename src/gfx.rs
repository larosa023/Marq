//! Small, safe-ish wrappers over the GDI+ flat API for anti-aliased shapes and strokes.

use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::*;

pub fn startup() {
    unsafe {
        let mut token = 0usize;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());
    }
}

/// ARGB color helper (0xAARRGGBB).
pub const fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub struct Graphics(*mut GpGraphics);

impl Graphics {
    pub fn from_hdc(hdc: HDC) -> Option<Graphics> {
        unsafe {
            let mut g = std::ptr::null_mut();
            GdipCreateFromHDC(hdc, &mut g);
            if g.is_null() {
                return None;
            }
            GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
            GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
            Some(Graphics(g))
        }
    }

    /// Restrict drawing to a rectangle (device coordinates), on top of the DC's own clip.
    pub fn clip(&self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            GdipSetClipRectI(self.0, x, y, w, h, CombineModeIntersect);
        }
    }

    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: u32) {
        unsafe {
            let pen = pen(color, width, false);
            GdipDrawLine(self.0, pen, x1, y1, x2, y2);
            GdipDeletePen(pen);
        }
    }

    pub fn fill_round_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, color: u32) {
        unsafe {
            let path = round_rect_path(x, y, w, h, r);
            let brush = solid(color);
            GdipFillPath(self.0, brush as *mut GpBrush, path);
            GdipDeleteBrush(brush as *mut GpBrush);
            GdipDeletePath(path);
        }
    }

    pub fn stroke_round_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, color: u32) {
        unsafe {
            let path = round_rect_path(x, y, w, h, r);
            let pen = pen(color, width, false);
            GdipDrawPath(self.0, pen, path);
            GdipDeletePen(pen);
            GdipDeletePath(path);
        }
    }

    pub fn fill_ellipse(&self, cx: f32, cy: f32, r: f32, color: u32) {
        unsafe {
            let brush = solid(color);
            GdipFillEllipse(self.0, brush as *mut GpBrush, cx - r, cy - r, r * 2.0, r * 2.0);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }

    /// Ellipse outline filling the box (x, y, w, h); a square box gives a circle.
    pub fn stroke_ellipse_box(&self, x: f32, y: f32, w: f32, h: f32, width: f32, color: u32) {
        unsafe {
            let pen = pen(color, width, false);
            GdipDrawEllipse(self.0, pen, x, y, w, h);
            GdipDeletePen(pen);
        }
    }

    pub fn stroke_ellipse(&self, cx: f32, cy: f32, r: f32, width: f32, color: u32) {
        unsafe {
            let pen = pen(color, width, false);
            GdipDrawEllipse(self.0, pen, cx - r, cy - r, r * 2.0, r * 2.0);
            GdipDeletePen(pen);
        }
    }

    /// A free-hand polyline with round caps and joins. With a translucent color the whole
    /// line is drawn in one pass, so overlapping segments don't darken (highlighter look).
    pub fn polyline(&self, pts: &[(f32, f32)], width: f32, color: u32, square: bool) {
        if pts.is_empty() {
            return;
        }
        unsafe {
            let pen = pen(color, width, square);
            if pts.len() == 1 {
                let (x, y) = pts[0];
                if square {
                    let b = solid(color);
                    GdipFillRectangle(self.0, b as *mut GpBrush, x - width / 2.0, y - width / 2.0, width, width);
                    GdipDeleteBrush(b as *mut GpBrush);
                } else {
                    let b = solid(color);
                    GdipFillEllipse(self.0, b as *mut GpBrush, x - width / 2.0, y - width / 2.0, width, width);
                    GdipDeleteBrush(b as *mut GpBrush);
                }
            } else {
                let p: Vec<PointF> = pts.iter().map(|&(x, y)| PointF { X: x, Y: y }).collect();
                GdipDrawLines(self.0, pen, p.as_ptr(), p.len() as i32);
            }
            GdipDeletePen(pen);
        }
    }

    /// Arrow along a path: 2 points = straight; more = a smooth curve through them,
    /// with the head pointing along the curve's final direction.
    pub fn arrow_path(&self, pts: &[(f32, f32)], width: f32, color: u32) {
        if pts.len() < 2 {
            return;
        }
        if pts.len() == 2 {
            let (a, b) = (pts[0], pts[1]);
            self.arrow(a.0, a.1, b.0, b.1, width, color);
            return;
        }
        let total: f32 = pts.windows(2).map(|w| dist(w[0], w[1])).sum();
        if total < 1.0 {
            return;
        }
        let head_len = (width * 4.5).min(total * 0.6).max(width * 2.0);
        let tip = pts[pts.len() - 1];
        // Walk back from the tip one head-length to find the head's direction.
        let mut back = pts[0];
        let mut acc = 0.0;
        for i in (1..pts.len()).rev() {
            let seg = dist(pts[i - 1], pts[i]);
            if acc + seg >= head_len {
                let t = (head_len - acc) / seg.max(1e-3);
                back = (pts[i].0 + (pts[i - 1].0 - pts[i].0) * t, pts[i].1 + (pts[i - 1].1 - pts[i].1) * t);
                break;
            }
            acc += seg;
        }
        let d = dist(back, tip).max(1e-3);
        let (ux, uy) = ((tip.0 - back.0) / d, (tip.1 - back.1) / d);
        // Shaft: the curve, ending a little inside the head.
        let stop = (tip.0 - ux * head_len * 0.8, tip.1 - uy * head_len * 0.8);
        let mut shaft: Vec<PointF> = pts[..pts.len() - 1]
            .iter()
            .filter(|&&p| dist(p, tip) > head_len * 0.9)
            .map(|&(x, y)| PointF { X: x, Y: y })
            .collect();
        if shaft.is_empty() {
            shaft.push(PointF { X: pts[0].0, Y: pts[0].1 });
        }
        shaft.push(PointF { X: stop.0, Y: stop.1 });
        unsafe {
            let pen = pen(color, width, false);
            if shaft.len() >= 3 {
                GdipDrawCurve2(self.0, pen, shaft.as_ptr(), shaft.len() as i32, 0.5);
            } else {
                GdipDrawLines(self.0, pen, shaft.as_ptr(), shaft.len() as i32);
            }
            GdipDeletePen(pen);
        }
        self.arrow_head(tip, (ux, uy), head_len, color);
    }

    fn arrow_head(&self, tip: (f32, f32), dir: (f32, f32), head_len: f32, color: u32) {
        let head_w = head_len * 0.62;
        let (bx, by) = (tip.0 - dir.0 * head_len, tip.1 - dir.1 * head_len);
        let (px, py) = (-dir.1, dir.0);
        let pts = [
            PointF { X: tip.0, Y: tip.1 },
            PointF { X: bx + px * head_w, Y: by + py * head_w },
            PointF { X: bx - px * head_w, Y: by - py * head_w },
        ];
        unsafe {
            let b = solid(color);
            GdipFillPolygon(self.0, b as *mut GpBrush, pts.as_ptr(), 3, FillModeAlternate);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }

    /// Straight arrow from (x1,y1) to (x2,y2) with a filled triangular head.
    pub fn arrow(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: u32) {
        let dx = x2 - x1;
        let dy = y2 - y1;
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1.0 {
            return;
        }
        let ux = dx / len;
        let uy = dy / len;
        let head_len = (width * 4.5).min(len * 0.6).max(width * 2.0);
        let head_w = head_len * 0.62;
        // Base of the head.
        let bx = x2 - ux * head_len;
        let by = y2 - uy * head_len;
        // Perpendicular.
        let px = -uy;
        let py = ux;
        unsafe {
            let pen = pen(color, width, false);
            // Stop the shaft a little inside the head so the round cap doesn't poke out.
            let sx = x2 - ux * head_len * 0.8;
            let sy = y2 - uy * head_len * 0.8;
            GdipDrawLine(self.0, pen, x1, y1, sx, sy);
            GdipDeletePen(pen);

            let pts = [
                PointF { X: x2, Y: y2 },
                PointF { X: bx + px * head_w, Y: by + py * head_w },
                PointF { X: bx - px * head_w, Y: by - py * head_w },
            ];
            let b = solid(color);
            GdipFillPolygon(self.0, b as *mut GpBrush, pts.as_ptr(), 3, FillModeAlternate);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteGraphics(self.0);
        }
    }
}

pub fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Drop points that don't change a path's shape by more than `eps` (Ramer–Douglas–Peucker).
pub fn simplify(pts: &[(f32, f32)], eps: f32) -> Vec<(f32, f32)> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (mut idx, mut dmax) = (0, 0.0);
    for (i, &p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        let d = line_dist(p, a, b);
        if d > dmax {
            idx = i;
            dmax = d;
        }
    }
    if dmax <= eps {
        return vec![a, b];
    }
    let mut left = simplify(&pts[..=idx], eps);
    let right = simplify(&pts[idx..], eps);
    left.pop();
    left.extend(right);
    left
}

/// Distance from p to the line through a and b (or to a, if a == b).
pub fn line_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-3 {
        return dist(p, a);
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

unsafe fn solid(color: u32) -> *mut GpSolidFill {
    let mut b = std::ptr::null_mut();
    GdipCreateSolidFill(color, &mut b);
    b
}

unsafe fn pen(color: u32, width: f32, square: bool) -> *mut GpPen {
    let mut p = std::ptr::null_mut();
    GdipCreatePen1(color, width, UnitPixel, &mut p);
    if square {
        GdipSetPenStartCap(p, LineCapSquare);
        GdipSetPenEndCap(p, LineCapSquare);
        GdipSetPenLineJoin(p, LineJoinRound);
    } else {
        GdipSetPenStartCap(p, LineCapRound);
        GdipSetPenEndCap(p, LineCapRound);
        GdipSetPenLineJoin(p, LineJoinRound);
    }
    p
}

unsafe fn round_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut GpPath {
    let mut path = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let d = r * 2.0;
    if d <= 0.0 {
        GdipAddPathRectangle(path, x, y, w, h);
        return path;
    }
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    path
}
