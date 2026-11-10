//! Markup editor: pen, marker, arrow (straight or curved), box, circle, text, numbers,
//! measure, zoom callout, blur and crop; five colors, three thicknesses, undo.
//! Every change re-copies the image to the clipboard (and the Gallery file) automatically.
//!
//! Smoothness: the image is pre-scaled once into a display buffer, the stroke in
//! progress is drawn at paint time (never baked into a full-size copy), and each
//! mouse move only repaints the pixels that actually changed.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, dist, line_dist, simplify, Graphics};
use crate::measure::{self, Scale};
use crate::util::*;

const CLASS: &str = "MarqEditor";
const TIMER_STATUS: usize = 1;
const TIMER_COPY: usize = 2;
/// Copy shortly after the last change, so quick strokes don't each land in Win+V history.
const COPY_DELAY_MS: u32 = 600;
/// From the text box: wParam 1 = keep the text, 0 = discard it.
const WM_APP_TEXT_DONE: u32 = WM_APP + 30;
const EM_SETMARGINS: u32 = 0x00D3;
const EM_SETCUEBANNER: u32 = 0x1501;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const BG: u32 = argb(255, 24, 24, 27);
const BAR: u32 = argb(255, 32, 32, 36);
const MUTED: u32 = argb(255, 150, 150, 162);
const BAR_H: i32 = 52;
const MARGIN: i32 = 12;
/// Thin / medium / thick.
const SIZE_MULT: [f32; 3] = [0.6, 1.0, 1.7];
/// Text label size on screen per thickness, in 96-DPI pixels.
const TEXT_PX: [i32; 3] = [16, 22, 32];
/// How much bigger the zoom callout shows its detail.
const ZOOM: f32 = 2.0;

const COLORS: [u32; 5] = [
    argb(255, 0xFF, 0x3B, 0x30), // red
    argb(255, 0xFF, 0xCC, 0x00), // yellow
    argb(255, 0x0A, 0x84, 0xFF), // blue
    argb(255, 0x34, 0xC7, 0x59), // green
    argb(255, 0x11, 0x11, 0x11), // black
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pen,
    Highlighter,
    Arrow,
    Rect,
    Ellipse,
    Text,
    Number,
    Measure,
    Zoom,
    Blur,
    Crop,
}

/// Toolbar order, name, and shortcut key.
const TOOLS: [(Tool, &str, char); 11] = [
    (Tool::Pen, "Pen", 'P'),
    (Tool::Highlighter, "Marker", 'H'),
    (Tool::Arrow, "Arrow", 'A'),
    (Tool::Rect, "Box", 'R'),
    (Tool::Ellipse, "Circle", 'O'),
    (Tool::Text, "Text", 'T'),
    (Tool::Number, "Number", 'N'),
    (Tool::Measure, "Measure", 'M'),
    (Tool::Zoom, "Zoom", 'Z'),
    (Tool::Blur, "Blur", 'B'),
    (Tool::Crop, "Crop", 'C'),
];

impl Tool {
    /// Tools you drag corner-to-corner (or end-to-end).
    fn two_point(self) -> bool {
        matches!(self, Tool::Rect | Tool::Ellipse | Tool::Measure | Tool::Zoom | Tool::Blur | Tool::Crop)
    }

    fn name(self) -> &'static str {
        TOOLS.iter().find(|t| t.0 == self).map_or("", |t| t.1)
    }

    fn key(self) -> char {
        TOOLS.iter().find(|t| t.0 == self).map_or(' ', |t| t.2)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Tool(Tool),
    Color(usize),
    Size,
    Undo,
    Copy,
}

struct Stroke {
    tool: Tool,
    color: u32,
    width: f32, // line width, text size, badge radius or blur block size, in image pixels
    pts: Vec<(f32, f32)>,
    text: String, // text label, badge number, or measured length
}

impl Stroke {
    /// How far ink can reach beyond the points.
    fn reach(&self) -> f32 {
        match self.tool {
            Tool::Arrow => self.width * 4.0 + 2.0,
            Tool::Measure => self.width * 8.0 + 30.0, // end ticks + label
            Tool::Blur | Tool::Crop | Tool::Zoom => 3.0,
            _ => self.width / 2.0 + 2.0,
        }
    }

    /// Normalized box between the first and last point.
    fn bounds(&self) -> (f32, f32, f32, f32) {
        let a = self.pts[0];
        let b = self.pts[self.pts.len() - 1];
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }
}

/// Image space → where it's drawn: p * k + (ox, oy).
#[derive(Clone, Copy)]
struct Map {
    ox: f32,
    oy: f32,
    k: f32,
}

impl Map {
    const IDENTITY: Map = Map { ox: 0.0, oy: 0.0, k: 1.0 };
    fn p(&self, p: (f32, f32)) -> (f32, f32) {
        (self.ox + p.0 * self.k, self.oy + p.1 * self.k)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Label,
    Scale, // typing the real length of the line just drawn
}

/// A text box being typed into.
struct TextEntry {
    edit: HWND,
    at: (f32, f32), // image-space top-left
    color: u32,
    purpose: Purpose,
}

/// A zoom callout whose detail has been picked and is now following the mouse.
struct ZoomPlacing {
    src: (f32, f32, f32, f32),
    at: (f32, f32),
    color: u32,
    width: f32,
}

struct Editor {
    base: Dib,              // current starting image (original snip, or the latest crop)
    committed: Dib,         // base + finished strokes, full resolution
    display: Option<Dib>,   // committed scaled to the view (None when scale == 1)
    back: Option<Dib>,      // persistent client-sized back buffer
    strokes: Vec<Stroke>,
    crops: Vec<(Dib, Vec<Stroke>)>, // what each crop replaced, for undo
    current: Option<Stroke>,
    current_dirty: RECT,    // screen-space bounds the current stroke has painted
    entry: Option<TextEntry>,
    pending_measure: Option<Stroke>, // waiting for its real length to be typed
    zoom: Option<ZoomPlacing>,
    cal: Option<Scale>,     // drawing scale for Measure
    recalibrate: bool,      // next Measure line sets a new scale
    note: Option<String>,   // one-off message in the toolbar
    edit_font: HFONT,
    edit_brush: HBRUSH,
    edit_brush_light: HBRUSH, // behind dark text colors, so black text stays readable
    tool: Tool,
    color: usize,
    size: usize,
    dpi: u32,
    font: HFONT,
    view: RECT,
    scale: f32,
    buttons: Vec<(Btn, RECT)>,
    bar_w: i32,
    hot: Option<usize>,
    copied: bool,
    copy_pending: bool,
    saved: Option<std::path::PathBuf>, // gallery file to keep in sync with the markup
    save_dirty: bool,
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.edit_font.into());
            let _ = DeleteObject(self.edit_brush.into());
            let _ = DeleteObject(self.edit_brush_light.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_ARROW, WNDCLASS_STYLES(0));
}

pub fn open(img: Image, saved: Option<std::path::PathBuf>) {
    let Some(base) = Dib::from_image(&img) else { return };
    let Some(committed) = Dib::from_image(&img) else { return };
    drop(img);

    let (cx, cy) = cursor_pos();
    let dpi = dpi_for_point(cx, cy);
    let work_area = work_area_for_point(cx, cy);
    let font = ui_font(sc(13, dpi), 500);
    let (buttons, bar_w) = build_buttons(font, dpi);

    // Size the window so the image fits in ~85% of the screen, never upscaled.
    let bar = sc(BAR_H, dpi);
    let margin = sc(MARGIN, dpi);
    let max_w = (rect_w(&work_area) as f32 * 0.85) as i32;
    let max_h = (rect_h(&work_area) as f32 * 0.85) as i32;
    let s = ((max_w - margin * 2) as f32 / base.w as f32)
        .min((max_h - bar - margin * 2) as f32 / base.h as f32)
        .clamp(0.05, 1.0);
    // Leave room for the hint text after the buttons when the image is small.
    let cw = ((base.w as f32 * s) as i32 + margin * 2).max(bar_w + sc(220, dpi));
    let ch = ((base.h as f32 * s) as i32 + bar + margin * 2).max(sc(240, dpi));

    let st = Box::new(Editor {
        base,
        committed,
        display: None,
        back: None,
        strokes: Vec::new(),
        crops: Vec::new(),
        current: None,
        current_dirty: RECT::default(),
        entry: None,
        pending_measure: None,
        zoom: None,
        cal: None,
        recalibrate: false,
        note: None,
        edit_font: font_px(sc(TEXT_PX[1], dpi), 600),
        edit_brush: unsafe { CreateSolidBrush(rgb_from_argb(argb(255, 44, 44, 50))) },
        edit_brush_light: unsafe { CreateSolidBrush(rgb_from_argb(argb(255, 236, 236, 240))) },
        tool: Tool::Pen,
        color: 0,
        size: 1,
        dpi,
        font,
        view: RECT::default(),
        scale: 1.0,
        buttons,
        bar_w,
        hot: None,
        copied: false,
        copy_pending: false,
        saved,
        save_dirty: false,
    });

    unsafe {
        let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
        let mut wr = rect(0, 0, cw, ch);
        let _ = AdjustWindowRectExForDpi(&mut wr, style, false, WINDOW_EX_STYLE(0), dpi);
        let ww = rect_w(&wr).min(rect_w(&work_area));
        let wh = rect_h(&wr).min(rect_h(&work_area));
        let x = work_area.left + (rect_w(&work_area) - ww) / 2;
        let y = work_area.top + (rect_h(&work_area) - wh) / 2;

        let cls = WStr::new(CLASS);
        let title = WStr::new("Marq");
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cls.p(),
            title.p(),
            style,
            x,
            y,
            ww,
            wh,
            None,
            None,
            Some(hinstance()),
            None,
        ) else {
            return;
        };
        set_state(hwnd, st);
        modern_chrome(hwnd, true, false);
        if let Some(st) = get_state::<Editor>(hwnd) {
            layout(hwnd, st);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        force_foreground(hwnd);
    }
}

/// Screen-pixel size of a tool at 96 DPI, before thickness and zoom are applied.
fn base_width(tool: Tool) -> f32 {
    match tool {
        Tool::Pen | Tool::Rect | Tool::Ellipse => 4.0,
        Tool::Highlighter => 22.0,
        Tool::Arrow => 5.0,
        Tool::Measure => 2.5,
        Tool::Number => 13.0, // badge radius
        Tool::Text => TEXT_PX[1] as f32,
        Tool::Zoom => 3.0,
        Tool::Blur => 12.0, // mosaic block size
        Tool::Crop => 1.5,
    }
}

fn stroke_width(st: &Editor, tool: Tool) -> f32 {
    let w = match tool {
        Tool::Text => TEXT_PX[st.size] as f32,
        Tool::Crop => base_width(tool),
        _ => base_width(tool) * SIZE_MULT[st.size],
    };
    scf(w, st.dpi) / st.scale
}

/// Smoothly anti-aliased font (no ClearType color fringes, since text gets baked into
/// the image).
fn font_px(px: i32, weight: i32) -> HFONT {
    let face = WStr::new("Segoe UI");
    unsafe {
        CreateFontW(
            -px.max(6), 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY, (DEFAULT_PITCH.0 | FF_SWISS.0) as u32, face.p(),
        )
    }
}

/// GDI text with a throwaway font of the given pixel size.
fn gdi_text(dc: HDC, text: &str, r: RECT, px: f32, weight: i32, color: u32, flags: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return; // see util::draw_text
    }
    let font = font_px(px.round() as i32, weight);
    unsafe {
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, rgb_from_argb(color));
        let mut w: Vec<u16> = text.encode_utf16().collect();
        let mut rr = r;
        DrawTextW(dc, &mut w, &mut rr, flags | DT_NOPREFIX);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
    }
}

fn text_size(dc: HDC, text: &str, px: f32, weight: i32) -> (i32, i32) {
    if text.is_empty() {
        return (0, 0);
    }
    let font = font_px(px.round() as i32, weight);
    unsafe {
        let old = SelectObject(dc, font.into());
        let w: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        (sz.cx, sz.cy)
    }
}

/// Snap the end of a line to the nearest 45°.
fn snap45(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    let step = std::f32::consts::FRAC_PI_4;
    let ang = (dy.atan2(dx) / step).round() * step;
    (a.0 + len * ang.cos(), a.1 + len * ang.sin())
}

/// The points an arrow is drawn through: straight if the drag was nearly straight,
/// otherwise a simplified version of the path (which then draws as a smooth curve).
fn arrow_points(pts: &[(f32, f32)], width: f32) -> Vec<(f32, f32)> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let bend = pts.iter().map(|&p| line_dist(p, a, b)).fold(0.0, f32::max);
    if bend < width * 2.5 {
        return vec![a, b];
    }
    let mut eps = width * 0.8;
    let mut out = simplify(pts, eps);
    while out.len() > 24 {
        eps *= 1.5;
        out = simplify(pts, eps);
    }
    out
}

/// Text on a badge: white, or near-black on light colors like yellow.
fn ink_on(color: u32) -> u32 {
    let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);
    if (r * 299 + g * 587 + b * 114) / 1000 > 170 { argb(255, 20, 20, 20) } else { argb(255, 255, 255, 255) }
}

/// Draw a vector stroke (everything except blur, crop and zoom, which work on pixels).
fn render(dc: HDC, s: &Stroke, m: Map) {
    let w = s.width * m.k;
    match s.tool {
        Tool::Pen | Tool::Highlighter | Tool::Arrow | Tool::Rect | Tool::Ellipse => {
            let Some(g) = Graphics::from_hdc(dc) else { return };
            match s.tool {
                Tool::Pen => {
                    let pts: Vec<_> = s.pts.iter().map(|&p| m.p(p)).collect();
                    g.polyline(&pts, w, s.color, false);
                }
                Tool::Highlighter => {
                    let pts: Vec<_> = s.pts.iter().map(|&p| m.p(p)).collect();
                    g.polyline(&pts, w, (s.color & 0x00FF_FFFF) | (0x70 << 24), true);
                }
                Tool::Arrow => {
                    let pts: Vec<_> = arrow_points(&s.pts, s.width).iter().map(|&p| m.p(p)).collect();
                    g.arrow_path(&pts, w, s.color);
                }
                Tool::Rect | Tool::Ellipse => {
                    let (x0, y0, x1, y1) = s.bounds();
                    let (a, b) = (m.p((x0, y0)), m.p((x1, y1)));
                    if s.tool == Tool::Rect {
                        g.stroke_round_rect(a.0, a.1, b.0 - a.0, b.1 - a.1, w * 0.6, w, s.color);
                    } else {
                        g.stroke_ellipse_box(a.0, a.1, b.0 - a.0, b.1 - a.1, w, s.color);
                    }
                }
                _ => {}
            }
        }
        Tool::Text => {
            let (x, y) = m.p(s.pts[0]);
            let r = rect(x.round() as i32, y.round() as i32, x as i32 + 20000, y as i32 + 20000);
            gdi_text(dc, &s.text, r, w, 600, s.color, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOCLIP);
        }
        Tool::Number => {
            let (cx, cy) = m.p(s.pts[0]);
            if let Some(g) = Graphics::from_hdc(dc) {
                g.fill_ellipse(cx, cy, w, s.color);
                g.stroke_ellipse(cx, cy, w, (w * 0.12).max(1.0), argb(220, 255, 255, 255));
            }
            let r = rect((cx - w) as i32, (cy - w) as i32, (cx + w).ceil() as i32 + 1, (cy + w).ceil() as i32 + 1);
            let px = if s.text.len() > 1 { w * 1.05 } else { w * 1.25 };
            gdi_text(dc, &s.text, r, px, 700, ink_on(s.color), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
        }
        Tool::Measure => render_measure(dc, s, m),
        Tool::Zoom | Tool::Blur | Tool::Crop => {}
    }
}

/// Dimension line: the line, perpendicular end ticks, and the length in a small pill.
fn render_measure(dc: HDC, s: &Stroke, m: Map) {
    let (a, b) = (m.p(s.pts[0]), m.p(s.pts[s.pts.len() - 1]));
    let w = (s.width * m.k).max(1.0);
    let len = dist(a, b).max(1e-3);
    let (ux, uy) = ((b.0 - a.0) / len, (b.1 - a.1) / len);
    let (nx, ny) = (-uy, ux);
    let tick = w * 4.0;
    if let Some(g) = Graphics::from_hdc(dc) {
        g.line(a.0, a.1, b.0, b.1, w, s.color);
        for p in [a, b] {
            g.line(p.0 - nx * tick, p.1 - ny * tick, p.0 + nx * tick, p.1 + ny * tick, w, s.color);
        }
    }
    if s.text.is_empty() {
        return;
    }
    let px = (w * 5.2).max(11.0);
    let (tw, th) = text_size(dc, &s.text, px, 600);
    let (mx, my) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let (pw, ph) = (tw as f32 + px * 0.9, th as f32 + px * 0.35);
    let (lx, ly) = (mx - pw / 2.0, my - ph / 2.0);
    if let Some(g) = Graphics::from_hdc(dc) {
        g.fill_round_rect(lx, ly, pw, ph, ph / 2.0, argb(235, 24, 24, 28));
        g.stroke_round_rect(lx, ly, pw, ph, ph / 2.0, (w * 0.5).max(1.0), s.color);
    }
    let r = rect(lx as i32, ly as i32, (lx + pw).ceil() as i32, (ly + ph).ceil() as i32);
    gdi_text(dc, &s.text, r, px, 600, argb(255, 245, 245, 250), DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
}

/// Apply a finished stroke to an image buffer.
fn apply_stroke(dib: &Dib, s: &Stroke) {
    match s.tool {
        Tool::Blur => pixelate(dib, s),
        Tool::Zoom => apply_zoom(dib, s),
        _ => render(dib.dc, s, Map::IDENTITY),
    }
    unsafe {
        let _ = GdiFlush();
    }
}

/// Mosaic the area: unreadable, and it can't be undone from the saved image.
fn pixelate(dib: &Dib, s: &Stroke) {
    unsafe {
        let _ = GdiFlush();
    }
    let (fx0, fy0, fx1, fy1) = s.bounds();
    let x0 = (fx0.floor() as i32).clamp(0, dib.w);
    let y0 = (fy0.floor() as i32).clamp(0, dib.h);
    let x1 = (fx1.ceil() as i32).clamp(0, dib.w);
    let y1 = (fy1.ceil() as i32).clamp(0, dib.h);
    let block = (s.width.round() as i32).max(4);
    let w = dib.w as usize;
    let px = dib.pixels_mut();
    let mut by = y0;
    while by < y1 {
        let by1 = (by + block).min(y1);
        let mut bx = x0;
        while bx < x1 {
            let bx1 = (bx + block).min(x1);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in by..by1 {
                for x in bx..bx1 {
                    let c = px[y as usize * w + x as usize];
                    r += (c >> 16) & 0xFF;
                    g += (c >> 8) & 0xFF;
                    b += c & 0xFF;
                    n += 1;
                }
            }
            if n > 0 {
                let avg = 0xFF00_0000 | ((r / n) << 16) | ((g / n) << 8) | (b / n);
                for y in by..by1 {
                    for x in bx..bx1 {
                        px[y as usize * w + x as usize] = avg;
                    }
                }
            }
            bx = bx1;
        }
        by = by1;
    }
}

/// Where a zoom callout's magnified copy goes, in image space (kept inside the image).
fn zoom_dest(src: (f32, f32, f32, f32), at: (f32, f32), img_w: f32, img_h: f32) -> (f32, f32, f32, f32) {
    let (sw, sh) = ((src.2 - src.0).max(1.0), (src.3 - src.1).max(1.0));
    let f = ZOOM.min(img_w * 0.95 / sw).min(img_h * 0.95 / sh).max(1.0);
    let (dw, dh) = (sw * f, sh * f);
    let x = (at.0 - dw / 2.0).clamp(0.0, (img_w - dw).max(0.0));
    let y = (at.1 - dh / 2.0).clamp(0.0, (img_h - dh).max(0.0));
    (x, y, x + dw, y + dh)
}

/// Callout outline and connector, mapped (shared by the preview and the final image).
fn zoom_frame(dc: HDC, src: (f32, f32, f32, f32), dst: (f32, f32, f32, f32), color: u32, width: f32, m: Map) {
    let (s0, s1) = (m.p((src.0, src.1)), m.p((src.2, src.3)));
    let (d0, d1) = (m.p((dst.0, dst.1)), m.p((dst.2, dst.3)));
    let w = width * m.k;
    if let Some(g) = Graphics::from_hdc(dc) {
        let sc_ = ((s0.0 + s1.0) / 2.0, (s0.1 + s1.1) / 2.0);
        let dc_ = ((d0.0 + d1.0) / 2.0, (d0.1 + d1.1) / 2.0);
        g.line(sc_.0, sc_.1, dc_.0, dc_.1, (w * 0.6).max(1.0), color);
        g.stroke_round_rect(s0.0, s0.1, s1.0 - s0.0, s1.1 - s0.1, 0.0, (w * 0.6).max(1.0), color);
    }
}

fn zoom_border(dc: HDC, dst: (f32, f32, f32, f32), color: u32, width: f32, m: Map) {
    let (d0, d1) = (m.p((dst.0, dst.1)), m.p((dst.2, dst.3)));
    if let Some(g) = Graphics::from_hdc(dc) {
        g.stroke_round_rect(d0.0, d0.1, d1.0 - d0.0, d1.1 - d0.1, 0.0, (width * m.k).max(1.0), color);
    }
}

fn apply_zoom(dib: &Dib, s: &Stroke) {
    if s.pts.len() < 3 {
        return;
    }
    let src = (s.pts[0].0, s.pts[0].1, s.pts[1].0, s.pts[1].1);
    let dst = zoom_dest(src, s.pts[2], dib.w as f32, dib.h as f32);
    unsafe {
        let _ = GdiFlush();
    }
    // Copy the detail first: the connector line may cross it.
    let r = rect(src.0.round() as i32, src.1.round() as i32, src.2.round() as i32, src.3.round() as i32);
    let detail = crop(dib.pixels_mut(), dib.w, dib.h, r);
    let Some(tmp) = Dib::from_image(&detail) else { return };
    zoom_frame(dib.dc, src, dst, s.color, s.width, Map::IDENTITY);
    unsafe {
        SetStretchBltMode(dib.dc, HALFTONE);
        let _ = SetBrushOrgEx(dib.dc, 0, 0, None);
        let _ = StretchBlt(
            dib.dc,
            dst.0.round() as i32,
            dst.1.round() as i32,
            (dst.2 - dst.0).round() as i32,
            (dst.3 - dst.1).round() as i32,
            Some(tmp.dc),
            0,
            0,
            tmp.w,
            tmp.h,
            SRCCOPY,
        );
        let _ = GdiFlush();
    }
    zoom_border(dib.dc, dst, s.color, s.width, Map::IDENTITY);
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

/// Toolbar buttons and the width they need.
fn build_buttons(font: HFONT, dpi: u32) -> (Vec<(Btn, RECT)>, i32) {
    let mut v = Vec::new();
    let bar = sc(BAR_H, dpi);
    let bh = sc(32, dpi);
    let y = (bar - bh) / 2;
    let mut x = sc(10, dpi);
    let gap = sc(2, dpi);
    let pad = sc(11, dpi);
    let iw = sc(34, dpi);
    for (t, _, _) in TOOLS {
        v.push((Btn::Tool(t), rect(x, y, x + iw, y + bh)));
        x += iw + gap;
    }
    x += sc(10, dpi);
    for i in 0..COLORS.len() {
        v.push((Btn::Color(i), rect(x, y, x + bh, y + bh)));
        x += bh + gap;
    }
    v.push((Btn::Size, rect(x, y, x + bh, y + bh)));
    x += bh + sc(10, dpi);
    let undo_w = text_w(font, "Undo") + pad * 2;
    v.push((Btn::Undo, rect(x, y, x + undo_w, y + bh)));
    x += undo_w + gap;
    let copy_w = text_w(font, "Copy").max(text_w(font, "Copied \u{2713}")) + pad * 2;
    v.push((Btn::Copy, rect(x, y, x + copy_w, y + bh)));
    x += copy_w + sc(10, dpi);
    (v, x)
}

fn text_w(font: HFONT, t: &str) -> i32 {
    unsafe {
        let dc = GetDC(None);
        let w = text_width(dc, t, font);
        ReleaseDC(None, dc);
        w
    }
}

/// Little line-art icon for each tool, centered in its button.
fn draw_icon(g: &Graphics, t: Tool, cx: f32, cy: f32, dpi: u32, c: u32) {
    let u = |v: f32| scf(v, dpi);
    let lw = u(1.7);
    match t {
        Tool::Pen => {
            g.line(cx - u(6.0), cy + u(6.0), cx + u(5.0), cy - u(5.0), u(2.2), c);
            g.fill_ellipse(cx - u(6.5), cy + u(6.5), u(1.4), c);
        }
        Tool::Highlighter => {
            g.polyline(&[(cx - u(5.5), cy + u(3.5)), (cx + u(5.5), cy - u(3.5))], u(6.0), (c & 0x00FF_FFFF) | (0x99 << 24), true);
        }
        Tool::Arrow => g.arrow_path(
            &[(cx - u(7.0), cy + u(5.0)), (cx - u(1.0), cy - u(5.0)), (cx + u(7.0), cy - u(4.0))],
            u(1.8),
            c,
        ),
        Tool::Rect => g.stroke_round_rect(cx - u(7.0), cy - u(5.5), u(14.0), u(11.0), u(1.5), lw, c),
        Tool::Ellipse => g.stroke_ellipse_box(cx - u(7.5), cy - u(5.5), u(15.0), u(11.0), lw, c),
        Tool::Text => {
            g.line(cx - u(6.0), cy - u(6.0), cx + u(6.0), cy - u(6.0), u(2.2), c);
            g.line(cx, cy - u(6.0), cx, cy + u(7.0), u(2.2), c);
        }
        Tool::Number => {
            g.stroke_ellipse(cx, cy, u(7.5), lw, c);
            g.line(cx + u(0.5), cy - u(4.0), cx + u(0.5), cy + u(4.0), lw, c);
            g.line(cx + u(0.5), cy - u(4.0), cx - u(2.0), cy - u(1.8), lw, c);
        }
        Tool::Measure => {
            g.stroke_round_rect(cx - u(8.5), cy - u(4.0), u(17.0), u(8.0), u(1.0), lw, c);
            for (i, x) in [-5.0f32, -1.7, 1.7, 5.0].iter().enumerate() {
                let len = if i % 2 == 0 { 3.5 } else { 2.0 };
                g.line(cx + u(*x), cy - u(4.0), cx + u(*x), cy - u(4.0 - len), u(1.3), c);
            }
        }
        Tool::Zoom => {
            g.stroke_ellipse(cx - u(1.5), cy - u(1.5), u(5.5), lw, c);
            g.line(cx + u(2.6), cy + u(2.6), cx + u(7.0), cy + u(7.0), u(2.4), c);
            g.line(cx - u(4.0), cy - u(1.5), cx + u(1.0), cy - u(1.5), u(1.4), c);
            g.line(cx - u(1.5), cy - u(4.0), cx - u(1.5), cy + u(1.0), u(1.4), c);
        }
        Tool::Blur => {
            let cell = u(4.0);
            for row in 0..3 {
                for col in 0..3 {
                    let a = [255u32, 110, 200, 140, 255, 90, 210, 120, 255][row * 3 + col];
                    let color = (c & 0x00FF_FFFF) | (a << 24);
                    let x = cx - u(6.5) + col as f32 * (cell + u(0.5));
                    let y = cy - u(6.5) + row as f32 * (cell + u(0.5));
                    g.fill_round_rect(x, y, cell, cell, u(0.8), color);
                }
            }
        }
        Tool::Crop => {
            g.polyline(&[(cx - u(4.0), cy - u(8.0)), (cx - u(4.0), cy + u(4.0)), (cx + u(8.0), cy + u(4.0))], lw, c, false);
            g.polyline(&[(cx - u(8.0), cy - u(4.0)), (cx + u(4.0), cy - u(4.0)), (cx + u(4.0), cy + u(8.0))], lw, c, false);
        }
    }
}

/// What the toolbar's hint area says: the hovered button, or how to use the current tool.
fn hint(st: &Editor) -> String {
    if let Some(n) = &st.note {
        return n.clone();
    }
    if let Some((b, _)) = st.hot.and_then(|i| st.buttons.get(i)) {
        return match b {
            Btn::Tool(t) => format!("{} ({})", t.name(), t.key()),
            Btn::Color(i) => format!("Color ({})", i + 1),
            Btn::Size => format!("Thickness: {} ([ and ])", ["thin", "medium", "thick"][st.size]),
            Btn::Undo => "Undo (Ctrl+Z)".into(),
            Btn::Copy => "Copy again (Ctrl+C)".into(),
        };
    }
    if st.zoom.is_some() {
        return "Click where the zoomed copy goes \u{00B7} Esc cancels".into();
    }
    match st.tool {
        Tool::Arrow => "Draw a curve to bend the arrow \u{00B7} Shift = straight".into(),
        Tool::Rect => "Shift = perfect square".into(),
        Tool::Ellipse => "Stretch for an oval \u{00B7} Shift = perfect circle".into(),
        Tool::Text => "Click, type, press Enter".into(),
        Tool::Number => format!("Click to place {}", count_numbers(st) + 1),
        Tool::Measure if st.cal.is_none() || st.recalibrate => {
            "Draw along a known length to set the scale".into()
        }
        Tool::Measure => "Shift = straight \u{00B7} click Measure again to reset the scale".into(),
        Tool::Zoom => "Drag around a small detail to magnify it".into(),
        Tool::Blur => "Drag over anything private".into(),
        Tool::Crop => "Drag the area to keep".into(),
        _ => String::new(),
    }
}

/// Badges already placed, including ones baked in before a crop.
fn count_numbers(st: &Editor) -> usize {
    st.strokes
        .iter()
        .chain(st.crops.iter().flat_map(|(_, v)| v))
        .filter(|s| s.tool == Tool::Number)
        .count()
}

fn layout(hwnd: HWND, st: &mut Editor) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    if rc.right <= 0 || rc.bottom <= 0 {
        return; // minimized
    }
    let dpi = st.dpi;
    let bar = sc(BAR_H, dpi);
    let margin = sc(MARGIN, dpi);

    // Fit image under the toolbar.
    let aw = (rc.right - margin * 2).max(1) as f32;
    let ah = (rc.bottom - bar - margin * 2).max(1) as f32;
    let s = (aw / st.base.w as f32).min(ah / st.base.h as f32).min(1.0).max(0.01);
    let w = ((st.base.w as f32 * s).round() as i32).max(1);
    let h = ((st.base.h as f32 * s).round() as i32).max(1);
    let left = (rc.right - w) / 2;
    let top = bar + margin + ((ah as i32 - h) / 2).max(0);
    st.view = rect(left, top, left + w, top + h);
    st.scale = if w == st.base.w && h == st.base.h { 1.0 } else { s };
    rebuild_display(st);

    // Back buffer follows the client size.
    let need_back = match &st.back {
        Some(b) => b.w != rc.right || b.h != rc.bottom,
        None => true,
    };
    if need_back {
        st.back = None; // free the old one first
        st.back = Dib::new(rc.right, rc.bottom);
    }
    let (buttons, bar_w) = build_buttons(st.font, dpi);
    st.buttons = buttons;
    st.bar_w = bar_w;
}

/// Re-create the scaled display copy of `committed` (only needed when scaled down).
fn rebuild_display(st: &mut Editor) {
    if st.scale >= 1.0 {
        st.display = None;
        return;
    }
    let (w, h) = (rect_w(&st.view), rect_h(&st.view));
    let reuse = matches!(&st.display, Some(d) if d.w == w && d.h == h);
    if !reuse {
        st.display = None;
        st.display = Dib::new(w, h);
    }
    if let Some(d) = &st.display {
        unsafe {
            SetStretchBltMode(d.dc, HALFTONE);
            let _ = SetBrushOrgEx(d.dc, 0, 0, None);
            let _ = StretchBlt(
                d.dc, 0, 0, w, h,
                Some(st.committed.dc), 0, 0, st.committed.w, st.committed.h, SRCCOPY,
            );
        }
    }
}

fn bar_rect(hwnd: HWND, st: &Editor) -> RECT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    rc.bottom = sc(BAR_H, st.dpi);
    rc
}

fn rebuild_committed(st: &mut Editor) {
    if st.committed.w != st.base.w || st.committed.h != st.base.h {
        match Dib::new(st.base.w, st.base.h) {
            Some(d) => st.committed = d,
            None => return,
        }
    }
    st.committed.copy_from(&st.base);
    for s in &st.strokes {
        apply_stroke(&st.committed, s);
    }
}

/// Update the Gallery copy with the current markup (in the background).
fn save_now(st: &mut Editor) {
    if !st.save_dirty {
        return;
    }
    st.save_dirty = false;
    if let Some(p) = &st.saved {
        unsafe {
            let _ = GdiFlush();
        }
        crate::gallery::save_async(st.committed.pixels_mut().to_vec(), st.committed.w, st.committed.h, p.clone());
    }
}

fn copy_now(hwnd: HWND, st: &mut Editor) {
    st.copy_pending = false;
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_COPY);
        let _ = GdiFlush();
    }
    save_now(st);
    if copy_to_clipboard(hwnd, st.committed.pixels_mut(), st.committed.w, st.committed.h) {
        st.copied = true;
        unsafe {
            SetTimer(Some(hwnd), TIMER_STATUS, 1400, None);
        }
    }
    let r = bar_rect(hwnd, st);
    invalidate(hwnd, r);
}

fn to_image_space(st: &Editor, x: i32, y: i32) -> (f32, f32) {
    (
        (x - st.view.left) as f32 / st.scale,
        (y - st.view.top) as f32 / st.scale,
    )
}

fn clamp_to_image(st: &Editor, p: (f32, f32)) -> (f32, f32) {
    (p.0.clamp(0.0, st.base.w as f32), p.1.clamp(0.0, st.base.h as f32))
}

fn view_map(st: &Editor) -> Map {
    Map { ox: st.view.left as f32, oy: st.view.top as f32, k: st.scale }
}

/// Screen-space bounds of some image-space points, padded for ink width.
fn screen_bounds(st: &Editor, pts: &[(f32, f32)], reach: f32) -> RECT {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let s = st.scale;
    let r = reach * s + 2.0;
    rect(
        (st.view.left as f32 + x0 * s - r).floor() as i32,
        (st.view.top as f32 + y0 * s - r).floor() as i32,
        (st.view.left as f32 + x1 * s + r).ceil() as i32,
        (st.view.top as f32 + y1 * s + r).ceil() as i32,
    )
}

fn invalidate(hwnd: HWND, r: RECT) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
    }
}

fn invalidate_all(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Committed changed: refresh the scaled copy, repaint the image, schedule a re-copy.
fn committed_changed(hwnd: HWND, st: &mut Editor) {
    unsafe {
        let _ = GdiFlush();
    }
    rebuild_display(st);
    invalidate(hwnd, st.view);
    invalidate(hwnd, bar_rect(hwnd, st)); // e.g. "Click to place 3"
    st.copy_pending = true;
    st.save_dirty = true;
    unsafe {
        SetTimer(Some(hwnd), TIMER_COPY, COPY_DELAY_MS, None);
    }
}

fn commit(hwnd: HWND, st: &mut Editor, s: Stroke) {
    apply_stroke(&st.committed, &s);
    st.strokes.push(s);
    committed_changed(hwnd, st);
}

fn busy(st: &Editor) -> bool {
    st.current.is_some() || st.entry.is_some() || st.zoom.is_some()
}

fn undo(hwnd: HWND, st: &mut Editor) {
    if busy(st) {
        return;
    }
    if st.strokes.pop().is_some() {
        rebuild_committed(st);
        committed_changed(hwnd, st);
    } else if let Some((base, strokes)) = st.crops.pop() {
        st.base = base;
        st.strokes = strokes;
        rebuild_committed(st);
        layout(hwnd, st);
        invalidate_all(hwnd);
        committed_changed(hwnd, st);
    }
}

fn do_crop(hwnd: HWND, st: &mut Editor, s: &Stroke) {
    let (x0, y0, x1, y1) = s.bounds();
    let r = rect(x0.round() as i32, y0.round() as i32, x1.round() as i32, y1.round() as i32);
    unsafe {
        let _ = GdiFlush();
    }
    let img = crop(st.committed.pixels_mut(), st.committed.w, st.committed.h, r);
    if img.w < 2 || img.h < 2 {
        return;
    }
    let Some(new_base) = Dib::from_image(&img) else { return };
    let old_base = std::mem::replace(&mut st.base, new_base);
    let old_strokes = std::mem::take(&mut st.strokes);
    st.crops.push((old_base, old_strokes));
    rebuild_committed(st);
    layout(hwnd, st);
    invalidate_all(hwnd);
    committed_changed(hwnd, st);
}

fn press(hwnd: HWND, st: &mut Editor, b: Btn) {
    st.note = None;
    match b {
        Btn::Tool(t) => {
            // Clicking Measure while it's already selected sets a new scale.
            if t == Tool::Measure && st.tool == Tool::Measure && st.cal.is_some() {
                st.recalibrate = true;
            }
            st.tool = t;
        }
        Btn::Color(i) => st.color = i,
        Btn::Size => st.size = (st.size + 1) % 3,
        Btn::Undo => undo(hwnd, st),
        Btn::Copy => copy_now(hwnd, st),
    }
    let r = bar_rect(hwnd, st);
    invalidate(hwnd, r);
}

/// Live-only previews for tools whose result isn't a plain shape.
fn draw_preview(dc: HDC, st: &Editor, s: &Stroke) {
    let m = view_map(st);
    let (x0, y0, x1, y1) = s.bounds();
    let (a, b) = (m.p((x0, y0)), m.p((x1, y1)));
    match s.tool {
        Tool::Blur | Tool::Zoom => {
            if let Some(g) = Graphics::from_hdc(dc) {
                g.fill_round_rect(a.0, a.1, b.0 - a.0, b.1 - a.1, 0.0, argb(60, 255, 255, 255));
                g.stroke_round_rect(a.0, a.1, b.0 - a.0, b.1 - a.1, 0.0, 1.5, argb(230, 255, 255, 255));
            }
        }
        Tool::Crop => {
            let v = st.view;
            let (l, t, r, btm) = (v.left as f32, v.top as f32, v.right as f32, v.bottom as f32);
            if let Some(g) = Graphics::from_hdc(dc) {
                let shade = argb(150, 0, 0, 0);
                g.fill_round_rect(l, t, r - l, a.1 - t, 0.0, shade);
                g.fill_round_rect(l, b.1, r - l, btm - b.1, 0.0, shade);
                g.fill_round_rect(l, a.1, a.0 - l, b.1 - a.1, 0.0, shade);
                g.fill_round_rect(b.0, a.1, r - b.0, b.1 - a.1, 0.0, shade);
                g.stroke_round_rect(a.0, a.1, b.0 - a.0, b.1 - a.1, 0.0, 1.5, argb(255, 255, 255, 255));
            }
        }
        Tool::Measure => {
            let px = dist(s.pts[0], s.pts[s.pts.len() - 1]) as f64;
            let cal = if st.recalibrate { None } else { st.cal };
            let mut live = Stroke { tool: s.tool, color: s.color, width: s.width, pts: s.pts.clone(), text: String::new() };
            live.text = measure::label(px, cal);
            render(dc, &live, m);
        }
        _ => render(dc, s, m),
    }
}

/// The magnified copy following the mouse, before it's placed.
fn draw_zoom_placing(dc: HDC, st: &Editor, z: &ZoomPlacing) {
    let m = view_map(st);
    let dst = zoom_dest(z.src, z.at, st.base.w as f32, st.base.h as f32);
    zoom_frame(dc, z.src, dst, z.color, z.width, m);
    let (d0, d1) = (m.p((dst.0, dst.1)), m.p((dst.2, dst.3)));
    unsafe {
        let _ = GdiFlush();
        SetStretchBltMode(dc, HALFTONE);
        let _ = SetBrushOrgEx(dc, 0, 0, None);
        let _ = StretchBlt(
            dc,
            d0.0.round() as i32,
            d0.1.round() as i32,
            (d1.0 - d0.0).round() as i32,
            (d1.1 - d0.1).round() as i32,
            Some(st.committed.dc),
            z.src.0.round() as i32,
            z.src.1.round() as i32,
            (z.src.2 - z.src.0).round().max(1.0) as i32,
            (z.src.3 - z.src.1).round().max(1.0) as i32,
            SRCCOPY,
        );
        let _ = GdiFlush();
    }
    zoom_border(dc, dst, z.color, z.width, m);
}

fn paint(hwnd: HWND, st: &mut Editor) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let rc = ps.rcPaint;
        let Some(back) = st.back.as_ref() else {
            let _ = EndPaint(hwnd, &ps);
            return;
        };
        let dpi = st.dpi;
        let bar = bar_rect(hwnd, st);
        let (pw, ph) = (rect_w(&rc), rect_h(&rc));

        // Background (only the dirty part).
        if let Some(g) = Graphics::from_hdc(back.dc) {
            g.clip(rc.left, rc.top, pw, ph);
            g.fill_round_rect(rc.left as f32, rc.top as f32, pw as f32, ph as f32, 0.0, BG);
        }

        // Toolbar.
        if intersect(rc, bar).is_some() {
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.fill_round_rect(0.0, 0.0, rect_w(&bar) as f32, rect_h(&bar) as f32, 0.0, BAR);
                for (i, (b, r)) in st.buttons.iter().enumerate() {
                    let (x, y, w, h) = (r.left as f32, r.top as f32, rect_w(r) as f32, rect_h(r) as f32);
                    let (cxp, cyp) = (x + w / 2.0, y + h / 2.0);
                    let hot = st.hot == Some(i);
                    match b {
                        Btn::Color(ci) => {
                            if *ci == st.color {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(2.0, dpi), argb(255, 255, 255, 255));
                            } else if hot {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(1.5, dpi), argb(90, 255, 255, 255));
                            }
                            g.fill_ellipse(cxp, cyp, w / 2.0 - scf(6.0, dpi), COLORS[*ci]);
                            if *ci == 4 {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - scf(6.0, dpi), 1.0, argb(110, 255, 255, 255));
                            }
                        }
                        Btn::Size => {
                            if hot {
                                g.fill_round_rect(x, y, w, h, h / 2.0, argb(36, 255, 255, 255));
                            }
                            let r = scf([2.5, 4.5, 7.0][st.size], dpi);
                            g.fill_ellipse(cxp, cyp, r, argb(255, 225, 225, 232));
                            g.stroke_ellipse(cxp, cyp, scf(9.0, dpi), 1.0, argb(70, 255, 255, 255));
                        }
                        Btn::Tool(t) => {
                            let active = *t == st.tool;
                            if active {
                                g.fill_round_rect(x, y, w, h, scf(8.0, dpi), ACCENT);
                            } else if hot {
                                g.fill_round_rect(x, y, w, h, scf(8.0, dpi), argb(36, 255, 255, 255));
                            }
                            let c = if active { argb(255, 255, 255, 255) } else { argb(255, 205, 205, 215) };
                            draw_icon(&g, *t, cxp, cyp, dpi, c);
                        }
                        Btn::Undo | Btn::Copy => {
                            if hot {
                                g.fill_round_rect(x, y, w, h, h / 2.0, argb(36, 255, 255, 255));
                            }
                        }
                    }
                }
            }
            for (b, r) in st.buttons.iter() {
                let (text, c) = match b {
                    Btn::Undo => ("Undo", argb(255, 205, 205, 215)),
                    Btn::Copy if st.copied => ("Copied \u{2713}", argb(255, 0x34, 0xC7, 0x59)),
                    Btn::Copy => ("Copy", argb(255, 205, 205, 215)),
                    _ => continue,
                };
                draw_text(back.dc, text, *r, st.font, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            // Hint after the last button, if there's room.
            let hint_left = st.bar_w;
            if bar.right - sc(12, dpi) > hint_left + sc(40, dpi) {
                let h = hint(st);
                let color = if st.note.is_some() { argb(255, 0xFF, 0x9F, 0x0A) } else { MUTED };
                draw_text(
                    back.dc, &h, rect(hint_left, 0, bar.right - sc(12, dpi), bar.bottom),
                    st.font, color, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
            }
        }

        // Image (from the pre-scaled copy) and anything in progress.
        if let Some(i) = intersect(rc, st.view) {
            let src = st.display.as_ref().unwrap_or(&st.committed);
            let _ = BitBlt(
                back.dc, i.left, i.top, rect_w(&i), rect_h(&i),
                Some(src.dc), i.left - st.view.left, i.top - st.view.top, SRCCOPY,
            );
            let saved = SaveDC(back.dc);
            let _ = IntersectClipRect(back.dc, i.left, i.top, i.right, i.bottom);
            if let Some(s) = &st.current {
                draw_preview(back.dc, st, s);
            }
            if let Some(s) = &st.pending_measure {
                render(back.dc, s, view_map(st));
            }
            if let Some(z) = &st.zoom {
                draw_zoom_placing(back.dc, st, z);
            }
            let _ = RestoreDC(back.dc, saved);
        }

        let _ = BitBlt(hdc, rc.left, rc.top, pw, ph, Some(back.dc), rc.left, rc.top, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn finish_stroke(hwnd: HWND, st: &mut Editor) {
    let Some(mut s) = st.current.take() else { return };
    unsafe {
        let _ = ReleaseCapture();
    }
    let dirty = st.current_dirty;
    st.current_dirty = RECT::default();
    let discard = |hwnd: HWND, st: &Editor, crop_like: bool| {
        invalidate(hwnd, if crop_like { st.view } else { dirty });
    };
    if s.tool.two_point() || s.tool == Tool::Arrow {
        let (x0, y0, x1, y1) = if s.tool == Tool::Arrow {
            s.pts.iter().fold((f32::MAX, f32::MAX, f32::MIN, f32::MIN), |b, p| {
                (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1))
            })
        } else {
            s.bounds()
        };
        let (w, h) = ((x1 - x0) * st.scale, (y1 - y0) * st.scale);
        let too_small = match s.tool {
            Tool::Arrow | Tool::Measure => (w * w + h * h).sqrt() < 6.0,
            _ => w < 4.0 || h < 4.0,
        };
        if too_small {
            // A click, not a drag: discard it.
            discard(hwnd, st, s.tool == Tool::Crop);
            return;
        }
    }
    match s.tool {
        Tool::Crop => do_crop(hwnd, st, &s),
        Tool::Zoom => {
            // Detail picked; now the magnified copy follows the mouse until a click.
            let (x0, y0, x1, y1) = s.bounds();
            st.zoom = Some(ZoomPlacing { src: (x0, y0, x1, y1), at: s.pts[1], color: s.color, width: s.width });
            invalidate(hwnd, st.view);
            invalidate(hwnd, bar_rect(hwnd, st));
        }
        Tool::Measure => {
            let px = dist(s.pts[0], s.pts[1]) as f64;
            if st.cal.is_none() || st.recalibrate {
                begin_scale_entry(hwnd, st, s);
            } else {
                s.text = measure::label(px, st.cal);
                commit(hwnd, st, s);
            }
        }
        Tool::Arrow => {
            s.pts = arrow_points(&s.pts, s.width);
            invalidate(hwnd, dirty);
            commit(hwnd, st, s);
        }
        _ => commit(hwnd, st, s),
    }
}

fn place_zoom(hwnd: HWND, st: &mut Editor) {
    let Some(z) = st.zoom.take() else { return };
    let s = Stroke {
        tool: Tool::Zoom,
        color: z.color,
        width: z.width,
        pts: vec![(z.src.0, z.src.1), (z.src.2, z.src.3), z.at],
        text: String::new(),
    };
    commit(hwnd, st, s);
    invalidate(hwnd, bar_rect(hwnd, st));
}

fn cancel_zoom(hwnd: HWND, st: &mut Editor) {
    if st.zoom.take().is_some() {
        invalidate(hwnd, st.view);
        invalidate(hwnd, bar_rect(hwnd, st));
    }
}

// ---------------------------------------------------------------------------
// Text boxes: for labels (click, type, Enter) and for the Measure scale (type the
// real length of the line you just drew). Clicking away keeps what was typed;
// Esc cancels.
// ---------------------------------------------------------------------------

fn create_edit(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, font: HFONT) -> Option<HWND> {
    let cls = WStr::new("EDIT");
    unsafe {
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cls.p(),
            None,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            x,
            y,
            w,
            h,
            Some(hwnd),
            None,
            Some(hinstance()),
            None,
        )
        .ok()?;
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        SendMessageW(edit, EM_SETMARGINS, Some(WPARAM(3)), Some(LPARAM(0))); // no inner margins
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, hwnd.0 as usize);
        let _ = SetFocus(Some(edit));
        Some(edit)
    }
}

fn begin_text(hwnd: HWND, st: &mut Editor, x: i32, y: i32) {
    let at = clamp_to_image(st, to_image_space(st, x, y));
    let px = sc(TEXT_PX[st.size], st.dpi);
    unsafe {
        let _ = DeleteObject(st.edit_font.into());
    }
    st.edit_font = font_px(px, 600);
    let h = px + sc(10, st.dpi);
    let w = (st.view.right - x).max(sc(160, st.dpi));
    if let Some(edit) = create_edit(hwnd, x, y, w, h, st.edit_font) {
        st.entry = Some(TextEntry { edit, at, color: COLORS[st.color], purpose: Purpose::Label });
    }
}

/// Ask for the real length of the measure line just drawn.
fn begin_scale_entry(hwnd: HWND, st: &mut Editor, s: Stroke) {
    let m = view_map(st);
    let (a, b) = (m.p(s.pts[0]), m.p(s.pts[1]));
    let (w, h) = (sc(210, st.dpi), sc(30, st.dpi));
    let x = (((a.0 + b.0) / 2.0) as i32 - w / 2).clamp(st.view.left.min(st.view.right - w), (st.view.right - w).max(st.view.left));
    let y = (((a.1 + b.1) / 2.0) as i32 + sc(14, st.dpi)).min(st.view.bottom - h).max(st.view.top);
    let at = to_image_space(st, x, y);
    let Some(edit) = create_edit(hwnd, x, y, w, h, st.font) else {
        // No box: keep the line in pixels.
        let mut s = s;
        s.text = measure::label(dist(s.pts[0], s.pts[1]) as f64, None);
        commit(hwnd, st, s);
        return;
    };
    let cue = WStr::new("Real length, e.g. 20' 6\" or 5 m");
    unsafe {
        SendMessageW(edit, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.0.as_ptr() as isize)));
    }
    st.pending_measure = Some(s);
    st.entry = Some(TextEntry { edit, at, color: argb(255, 245, 245, 250), purpose: Purpose::Scale });
    invalidate(hwnd, st.view);
    invalidate(hwnd, bar_rect(hwnd, st));
}

fn end_text(hwnd: HWND, st: &mut Editor, keep: bool, refocus: bool) {
    let Some(entry) = st.entry.take() else { return };
    let text = unsafe {
        let n = GetWindowTextLengthW(entry.edit).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(entry.edit, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    };
    unsafe {
        // Only take focus back if the text box still had it. Otherwise (e.g. the user
        // just pressed the snip shortcut) grabbing focus would re-activate this window
        // and close the new snip overlay.
        let had_focus = GetFocus() == entry.edit;
        let _ = DestroyWindow(entry.edit);
        if refocus && had_focus {
            let _ = SetFocus(Some(hwnd));
        }
    }
    match entry.purpose {
        Purpose::Label => {
            if keep && !text.trim().is_empty() {
                let s = Stroke {
                    tool: Tool::Text,
                    color: entry.color,
                    // Same pixel size as the text box.
                    width: sc(TEXT_PX[st.size], st.dpi) as f32 / st.scale,
                    pts: vec![entry.at],
                    text,
                };
                commit(hwnd, st, s);
            }
        }
        Purpose::Scale => {
            if let Some(mut s) = st.pending_measure.take() {
                if !keep {
                    // Esc: drop the line; the scale stays as it was.
                    st.recalibrate = false;
                    invalidate(hwnd, st.view);
                    invalidate(hwnd, bar_rect(hwnd, st));
                    return;
                }
                let px = dist(s.pts[0], s.pts[1]) as f64;
                let typed = if keep { measure::parse(&text) } else { None };
                match typed {
                    Some((value, units)) => {
                        st.cal = Some(Scale { units, per_px: value / px.max(1e-6) });
                        st.recalibrate = false;
                        st.note = None;
                        s.text = measure::format(value, units);
                    }
                    None => {
                        if keep && !text.trim().is_empty() {
                            st.note = Some("Couldn't read that length \u{2014} try 20' 6\" or 5 m".into());
                        }
                        s.text = measure::label(px, if st.recalibrate { None } else { st.cal });
                    }
                }
                commit(hwnd, st, s);
            }
        }
    }
    invalidate(hwnd, st.view);
    invalidate(hwnd, bar_rect(hwnd, st));
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    parent: usize,
) -> LRESULT {
    let parent = HWND(parent as *mut _);
    match msg {
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize || wp.0 == VK_ESCAPE.0 as usize => {
            let keep = wp.0 == VK_RETURN.0 as usize;
            let _ = PostMessageW(Some(parent), WM_APP_TEXT_DONE, WPARAM(keep as usize), LPARAM(0));
            LRESULT(0)
        }
        // Swallow the matching characters so the box doesn't beep.
        WM_CHAR if wp.0 == 0x0D || wp.0 == 0x1B => LRESULT(0),
        WM_KILLFOCUS => {
            let _ = PostMessageW(Some(parent), WM_APP_TEXT_DONE, WPARAM(1), LPARAM(0));
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(edit_proc), 1);
            DefSubclassProc(hwnd, msg, wp, lp)
        }
        _ => DefSubclassProc(hwnd, msg, wp, lp),
    }
}

fn set_size(hwnd: HWND, st: &mut Editor, size: usize) {
    st.size = size.min(2);
    invalidate(hwnd, bar_rect(hwnd, st));
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = get_state::<Editor>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            end_text(hwnd, st, true, false);
            layout(hwnd, st);
            invalidate_all(hwnd);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // Never narrower than the toolbar.
            let mut wr = rect(0, 0, st.bar_w.max(sc(400, st.dpi)), sc(260, st.dpi));
            let _ = AdjustWindowRectExForDpi(&mut wr, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0), st.dpi);
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: rect_w(&wr), y: rect_h(&wr) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            end_text(hwnd, st, true, false);
            st.dpi = (hiword(wp.0) as u32).max(96);
            let _ = DeleteObject(st.font.into());
            st.font = ui_font(sc(13, st.dpi), 500);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd, None, r.left, r.top, rect_w(r), rect_h(r),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            layout(hwnd, st);
            invalidate_all(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wp.0 as *mut _);
            let color = st.entry.as_ref().map_or(COLORS[st.color], |e| e.color);
            let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);
            let dark = (r * 299 + g * 587 + b * 114) / 1000 < 0x60;
            SetTextColor(hdc, rgb_from_argb(color));
            if dark {
                SetBkColor(hdc, rgb_from_argb(argb(255, 236, 236, 240)));
                LRESULT(st.edit_brush_light.0 as isize)
            } else {
                SetBkColor(hdc, rgb_from_argb(argb(255, 44, 44, 50)));
                LRESULT(st.edit_brush.0 as isize)
            }
        }
        WM_APP_TEXT_DONE => {
            end_text(hwnd, st, wp.0 != 0, true);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Over the text box: let it show its own I-beam.
            if HWND(wp.0 as *mut _) == hwnd && loword(lp.0 as usize) as u32 == HTCLIENT {
                let (x, y) = cursor_pos();
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                let c = if pt_in(&st.view, p.x, p.y) {
                    match st.tool {
                        Tool::Text => IDC_IBEAM,
                        _ if st.zoom.is_some() => IDC_HAND,
                        _ => IDC_CROSS,
                    }
                } else if st.buttons.iter().any(|(_, r)| pt_in(r, p.x, p.y)) {
                    IDC_HAND
                } else {
                    IDC_ARROW
                };
                SetCursor(LoadCursorW(None, c).ok());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONDOWN => {
            if st.entry.is_some() {
                // First click away from a text box just places the text.
                end_text(hwnd, st, true, true);
                return LRESULT(0);
            }
            let (x, y) = lparam_point(lp);
            if st.zoom.is_some() {
                if pt_in(&st.view, x, y) {
                    place_zoom(hwnd, st);
                } else {
                    cancel_zoom(hwnd, st);
                }
                return LRESULT(0);
            }
            if let Some((b, _)) = st.buttons.iter().copied().find(|(_, r)| pt_in(r, x, y)) {
                press(hwnd, st, b);
                return LRESULT(0);
            }
            if pt_in(&st.view, x, y) && st.current.is_none() {
                st.note = None;
                match st.tool {
                    Tool::Text => {
                        begin_text(hwnd, st, x, y);
                        return LRESULT(0);
                    }
                    Tool::Number => {
                        let p = clamp_to_image(st, to_image_space(st, x, y));
                        let s = Stroke {
                            tool: Tool::Number,
                            color: COLORS[st.color],
                            width: stroke_width(st, Tool::Number),
                            pts: vec![p],
                            text: (count_numbers(st) + 1).to_string(),
                        };
                        commit(hwnd, st, s);
                        return LRESULT(0);
                    }
                    _ => {}
                }
                let p = to_image_space(st, x, y);
                let p = if st.tool.two_point() { clamp_to_image(st, p) } else { p };
                let s = Stroke {
                    tool: st.tool,
                    color: COLORS[st.color],
                    // Same thickness on screen no matter how far the view is zoomed out.
                    width: stroke_width(st, st.tool),
                    // Pen/marker start as a single point, so a click leaves a dot.
                    pts: if st.tool.two_point() { vec![p, p] } else { vec![p] },
                    text: String::new(),
                };
                st.current_dirty = screen_bounds(st, &s.pts, s.reach());
                st.current = Some(s);
                SetCapture(hwnd);
                invalidate(hwnd, st.current_dirty);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let hot = st.buttons.iter().position(|(_, r)| pt_in(r, x, y));
            if hot != st.hot {
                st.hot = hot;
                invalidate(hwnd, bar_rect(hwnd, st));
            }
            if st.zoom.is_some() {
                let p = clamp_to_image(st, to_image_space(st, x, y));
                if let Some(z) = st.zoom.as_mut() {
                    z.at = p;
                }
                invalidate(hwnd, st.view);
                return LRESULT(0);
            }
            if st.current.is_none() {
                return LRESULT(0);
            }
            let raw = to_image_space(st, x, y);
            let tool = st.current.as_ref().map_or(Tool::Pen, |s| s.tool);
            let two = tool.two_point();
            let mut p = if two { clamp_to_image(st, raw) } else { raw };
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let origin = st.current.as_ref().unwrap().pts[0];
            if shift {
                match tool {
                    // Perfect circle / square.
                    Tool::Rect | Tool::Ellipse => {
                        let (dx, dy) = (p.0 - origin.0, p.1 - origin.1);
                        let side = dx.abs().max(dy.abs());
                        p = clamp_to_image(st, (origin.0 + side * dx.signum(), origin.1 + side * dy.signum()));
                    }
                    // Straight, at 0/45/90°.
                    Tool::Measure | Tool::Arrow => p = clamp_to_image(st, snap45(origin, p)),
                    _ => {}
                }
            }
            let dirty = {
                let s = st.current.as_mut().unwrap();
                let last = *s.pts.last().unwrap();
                if (last.0 - p.0).abs() < 0.5 && (last.1 - p.1).abs() < 0.5 {
                    return LRESULT(0); // sub-pixel jitter
                }
                if two {
                    let n = s.pts.len() - 1;
                    s.pts[n] = p;
                    None
                } else if tool == Tool::Arrow && shift {
                    s.pts = vec![origin, p];
                    None
                } else {
                    s.pts.push(p);
                    if tool == Tool::Arrow { None } else { Some([last, p]) }
                }
            };
            let s = st.current.as_ref().unwrap();
            let reach = s.reach();
            // Crop shading, measure labels and smoothed arrow curves can reach anywhere.
            let r = if matches!(tool, Tool::Crop | Tool::Measure | Tool::Arrow) {
                st.view
            } else {
                match dirty {
                    // Free-hand: only the newest segment changed.
                    Some(seg) => screen_bounds(st, &seg, reach),
                    // Shapes and arrows: old and new outline.
                    None => {
                        let now = screen_bounds(st, &s.pts, reach);
                        let r = union(st.current_dirty, now);
                        st.current_dirty = now;
                        r
                    }
                }
            };
            invalidate(hwnd, r);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            finish_stroke(hwnd, st);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            cancel_zoom(hwnd, st);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // Capture stolen mid-stroke (e.g. alt-tab): keep what was drawn.
            if st.current.is_some() && HWND(lp.0 as *mut _) != hwnd {
                finish_stroke(hwnd, st);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let key = VIRTUAL_KEY(wp.0 as u16);
            if key == VK_ESCAPE {
                if st.zoom.is_some() {
                    cancel_zoom(hwnd, st);
                } else if st.current.is_none() {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            if ctrl {
                match key {
                    VK_Z => undo(hwnd, st),
                    VK_C => copy_now(hwnd, st),
                    _ => {}
                }
                return LRESULT(0);
            }
            if busy(st) {
                return LRESULT(0);
            }
            match key {
                k if (VK_1.0..=VK_5.0).contains(&k.0) => press(hwnd, st, Btn::Color((k.0 - VK_1.0) as usize)),
                VK_OEM_4 => set_size(hwnd, st, st.size.saturating_sub(1)), // [
                VK_OEM_6 => set_size(hwnd, st, st.size + 1),              // ]
                k => {
                    let c = char::from_u32(k.0 as u32).unwrap_or(' ');
                    // A tool's key re-pressed does nothing (only clicking Measure resets its scale).
                    if let Some((t, _, _)) = TOOLS.iter().find(|t| t.2 == c && t.0 != st.tool) {
                        press(hwnd, st, Btn::Tool(*t));
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_STATUS => {
                    let _ = KillTimer(Some(hwnd), TIMER_STATUS);
                    st.copied = false;
                    invalidate(hwnd, bar_rect(hwnd, st));
                }
                TIMER_COPY => copy_now(hwnd, st),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Closing right after a change: still put the final image on the clipboard.
            end_text(hwnd, st, true, false);
            st.zoom = None;
            if st.current.as_ref().map_or(false, |s| s.tool == Tool::Measure) {
                // Don't open a scale box on a closing window: keep the line as measured.
                let mut s = st.current.take().unwrap();
                let _ = ReleaseCapture();
                let px = dist(s.pts[0], s.pts[s.pts.len() - 1]) as f64;
                s.text = measure::label(px, if st.recalibrate { None } else { st.cal });
                commit(hwnd, st, s);
            } else if st.current.is_some() {
                finish_stroke(hwnd, st);
            }
            if st.copy_pending {
                let _ = GdiFlush();
                copy_to_clipboard(crate::host(), st.committed.pixels_mut(), st.committed.w, st.committed.h);
                st.copy_pending = false;
            }
            save_now(st);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = KillTimer(Some(hwnd), TIMER_STATUS);
            let _ = KillTimer(Some(hwnd), TIMER_COPY);
            drop(take_state::<Editor>(hwnd));
            trim_memory();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
