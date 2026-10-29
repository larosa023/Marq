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
