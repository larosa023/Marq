//! Markup editor: pen, marker, arrow, box, text, blur and crop, five colors, undo.
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

use crate::gfx::{argb, Graphics};
use crate::util::*;

const CLASS: &str = "MarqEditor";
const TIMER_STATUS: usize = 1;
const TIMER_COPY: usize = 2;
/// Copy shortly after the last change, so quick strokes don't each land in Win+V history.
const COPY_DELAY_MS: u32 = 600;
/// From the text box: wParam 1 = keep the text, 0 = discard it.
const WM_APP_TEXT_DONE: u32 = WM_APP + 30;
const EM_SETMARGINS: u32 = 0x00D3;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const BG: u32 = argb(255, 24, 24, 27);
const BAR: u32 = argb(255, 32, 32, 36);
const BAR_H: i32 = 52;
const MARGIN: i32 = 12;
/// Text label size on screen, in 96-DPI pixels.
const TEXT_PX: i32 = 22;

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
    Text,
    Blur,
    Crop,
}

const TOOLS: [(Tool, &str); 7] = [
    (Tool::Pen, "Pen"),
    (Tool::Highlighter, "Marker"),
    (Tool::Arrow, "Arrow"),
    (Tool::Rect, "Box"),
    (Tool::Text, "Text"),
    (Tool::Blur, "Blur"),
    (Tool::Crop, "Crop"),
];

impl Tool {
    /// Tools you drag corner-to-corner (or end-to-end).
    fn two_point(self) -> bool {
        matches!(self, Tool::Arrow | Tool::Rect | Tool::Blur | Tool::Crop)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Tool(Tool),
    Color(usize),
    Undo,
    Copy,
}

struct Stroke {
    tool: Tool,
    color: u32,
    width: f32, // line width, text size or blur block size, in image pixels
    pts: Vec<(f32, f32)>,
    text: String,
}

impl Stroke {
    /// How far ink can reach beyond the points (arrowheads are wider than the shaft).
    fn reach(&self) -> f32 {
        match self.tool {
            Tool::Arrow => self.width * 3.5 + 2.0,
            Tool::Blur | Tool::Crop => 3.0,
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

/// A text box being typed into.
struct TextEntry {
    edit: HWND,
    at: (f32, f32), // image-space top-left
    color: u32,
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
    edit_font: HFONT,
    edit_brush: HBRUSH,
    tool: Tool,
    color: usize,
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
    let cw = ((base.w as f32 * s) as i32 + margin * 2).max(bar_w);
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
        edit_font: label_font(sc(TEXT_PX, dpi)),
        edit_brush: unsafe { CreateSolidBrush(rgb_from_argb(argb(255, 44, 44, 50))) },
        tool: Tool::Pen,
        color: 0,
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

fn stroke_width(tool: Tool, dpi: u32) -> f32 {
    let base = match tool {
        Tool::Pen | Tool::Rect => 4.0,
        Tool::Highlighter => 22.0,
        Tool::Arrow => 5.0,
        Tool::Text => TEXT_PX as f32,
        Tool::Blur => 12.0, // mosaic block size
        Tool::Crop => 1.5,
    };
    scf(base, dpi)
}

/// Bold, smoothly anti-aliased font for text labels (no ClearType color fringes,
/// since labels are baked into the image).
fn label_font(px: i32) -> HFONT {
    let face = WStr::new("Segoe UI");
    unsafe {
        CreateFontW(
            -px.max(6), 0, 0, 0, 600, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY, (DEFAULT_PITCH.0 | FF_SWISS.0) as u32, face.p(),
        )
    }
}

/// Draw shapes that GDI+ handles (pen, marker, arrow, box) — used for both the live
/// preview and the final image.
fn draw_stroke(g: &Graphics, s: &Stroke) {
    match s.tool {
        Tool::Pen => g.polyline(&s.pts, s.width, s.color, false),
        Tool::Highlighter => {
            let c = (s.color & 0x00FF_FFFF) | (0x70 << 24);
            g.polyline(&s.pts, s.width, c, true)
        }
        Tool::Arrow => {
            if s.pts.len() >= 2 {
                let (a, b) = (s.pts[0], s.pts[s.pts.len() - 1]);
                g.arrow(a.0, a.1, b.0, b.1, s.width, s.color);
            }
        }
        Tool::Rect => {
            let (x0, y0, x1, y1) = s.bounds();
            g.stroke_round_rect(x0, y0, x1 - x0, y1 - y0, s.width * 0.6, s.width, s.color);
        }
        Tool::Text | Tool::Blur | Tool::Crop => {}
    }
}

/// Apply a finished stroke to an image buffer.
fn apply_stroke(dib: &Dib, s: &Stroke) {
    match s.tool {
        Tool::Blur => pixelate(dib, s),
        Tool::Text => draw_label(dib, s),
        _ => {
            if let Some(g) = Graphics::from_hdc(dib.dc) {
                draw_stroke(&g, s);
            }
        }
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

fn draw_label(dib: &Dib, s: &Stroke) {
    let font = label_font(s.width.round() as i32);
    let (x, y) = s.pts[0];
    let r = rect(x.round() as i32, y.round() as i32, dib.w + 4000, dib.h + 4000);
    unsafe {
        let old = SelectObject(dib.dc, font.into());
        SetBkMode(dib.dc, TRANSPARENT);
        SetTextColor(dib.dc, rgb_from_argb(s.color));
        let mut w: Vec<u16> = s.text.encode_utf16().collect();
        let mut rr = r;
        DrawTextW(dib.dc, &mut w, &mut rr, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
        SelectObject(dib.dc, old);
        let _ = DeleteObject(font.into());
        let _ = GdiFlush();
    }
}

fn text_w(font: HFONT, t: &str) -> i32 {
    unsafe {
        let dc = GetDC(None);
        let w = text_width(dc, t, font);
        ReleaseDC(None, dc);
        w
    }
}

/// Toolbar buttons and the width they need.
fn build_buttons(font: HFONT, dpi: u32) -> (Vec<(Btn, RECT)>, i32) {
    let mut v = Vec::new();
    let bar = sc(BAR_H, dpi);
    let bh = sc(32, dpi);
    let y = (bar - bh) / 2;
    let mut x = sc(10, dpi);
    let gap = sc(3, dpi);
    let pad = sc(11, dpi);
    for (t, label) in TOOLS {
        let w = text_w(font, label) + pad * 2;
        v.push((Btn::Tool(t), rect(x, y, x + w, y + bh)));
        x += w + gap;
    }
    x += sc(10, dpi);
    for i in 0..COLORS.len() {
        v.push((Btn::Color(i), rect(x, y, x + bh, y + bh)));
        x += bh + gap;
    }
    x += sc(10, dpi);
    let undo_w = text_w(font, "Undo") + pad * 2;
    v.push((Btn::Undo, rect(x, y, x + undo_w, y + bh)));
    x += undo_w + gap;
    let copy_w = text_w(font, "Copy").max(text_w(font, "Copied \u{2713}")) + pad * 2;
    v.push((Btn::Copy, rect(x, y, x + copy_w, y + bh)));
    x += copy_w + sc(10, dpi);
    (v, x)
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

fn label(b: Btn, copied: bool) -> &'static str {
    match b {
        Btn::Tool(t) => TOOLS.iter().find(|(x, _)| *x == t).map(|(_, l)| *l).unwrap_or(""),
        Btn::Undo => "Undo",
        Btn::Copy if copied => "Copied \u{2713}",
        Btn::Copy => "Copy",
        Btn::Color(_) => "",
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
    unsafe {
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
    }
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
    st.copy_pending = true;
    st.save_dirty = true;
    unsafe {
        SetTimer(Some(hwnd), TIMER_COPY, COPY_DELAY_MS, None);
    }
}

fn undo(hwnd: HWND, st: &mut Editor) {
    if st.current.is_some() || st.entry.is_some() {
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
    match b {
        Btn::Tool(t) => st.tool = t,
        Btn::Color(i) => st.color = i,
        Btn::Undo => undo(hwnd, st),
        Btn::Copy => copy_now(hwnd, st),
    }
    let r = bar_rect(hwnd, st);
    invalidate(hwnd, r);
}

/// Live-only previews for tools whose result isn't a plain shape.
fn draw_preview(g: &Graphics, st: &Editor, s: &Stroke) {
    let (x0, y0, x1, y1) = s.bounds();
    let px = 1.0 / st.scale; // one screen pixel, in image units
    match s.tool {
        Tool::Blur => {
            g.fill_round_rect(x0, y0, x1 - x0, y1 - y0, 0.0, argb(70, 255, 255, 255));
            g.stroke_round_rect(x0, y0, x1 - x0, y1 - y0, 0.0, 1.5 * px, argb(230, 255, 255, 255));
        }
        Tool::Crop => {
            let (w, h) = (st.base.w as f32, st.base.h as f32);
            let shade = argb(150, 0, 0, 0);
            g.fill_round_rect(0.0, 0.0, w, y0, 0.0, shade);
            g.fill_round_rect(0.0, y1, w, h - y1, 0.0, shade);
            g.fill_round_rect(0.0, y0, x0, y1 - y0, 0.0, shade);
            g.fill_round_rect(x1, y0, w - x1, y1 - y0, 0.0, shade);
            g.stroke_round_rect(x0, y0, x1 - x0, y1 - y0, 0.0, 1.5 * px, argb(255, 255, 255, 255));
        }
        _ => draw_stroke(g, s),
    }
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
                    match b {
                        Btn::Color(ci) => {
                            let cxp = x + w / 2.0;
                            let cyp = y + h / 2.0;
                            if *ci == st.color {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(2.0, dpi), argb(255, 255, 255, 255));
                            } else if st.hot == Some(i) {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(1.5, dpi), argb(90, 255, 255, 255));
                            }
                            g.fill_ellipse(cxp, cyp, w / 2.0 - scf(6.0, dpi), COLORS[*ci]);
                            if *ci == 4 {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - scf(6.0, dpi), 1.0, argb(110, 255, 255, 255));
                            }
                        }
                        _ => {
                            let active = matches!(b, Btn::Tool(t) if *t == st.tool);
                            if active {
                                g.fill_round_rect(x, y, w, h, h / 2.0, ACCENT);
                            } else if st.hot == Some(i) {
                                g.fill_round_rect(x, y, w, h, h / 2.0, argb(36, 255, 255, 255));
                            }
                        }
                    }
                }
            }
            for (b, r) in st.buttons.iter() {
                if let Btn::Color(_) = b {
                    continue;
                }
                let active = matches!(b, Btn::Tool(t) if *t == st.tool);
                let c = if active {
                    argb(255, 255, 255, 255)
                } else if *b == Btn::Copy && st.copied {
                    argb(255, 0x34, 0xC7, 0x59)
                } else {
                    argb(255, 205, 205, 215)
                };
                draw_text(back.dc, label(*b, st.copied), *r, st.font, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
        }

        // Image (from the pre-scaled copy) and the stroke in progress.
        if let Some(i) = intersect(rc, st.view) {
            let src = st.display.as_ref().unwrap_or(&st.committed);
            let _ = BitBlt(
                back.dc, i.left, i.top, rect_w(&i), rect_h(&i),
                Some(src.dc), i.left - st.view.left, i.top - st.view.top, SRCCOPY,
            );
            if let Some(s) = &st.current {
                if let Some(g) = Graphics::from_hdc(back.dc) {
                    g.clip(i.left, i.top, rect_w(&i), rect_h(&i));
                    g.set_transform(st.view.left as f32, st.view.top as f32, st.scale);
                    draw_preview(&g, st, s);
                }
            }
        }

        let _ = BitBlt(hdc, rc.left, rc.top, pw, ph, Some(back.dc), rc.left, rc.top, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn finish_stroke(hwnd: HWND, st: &mut Editor) {
    let Some(s) = st.current.take() else { return };
    unsafe {
        let _ = ReleaseCapture();
    }
    let dirty = st.current_dirty;
    st.current_dirty = RECT::default();
    if s.tool.two_point() {
        let (x0, y0, x1, y1) = s.bounds();
        let (w, h) = ((x1 - x0) * st.scale, (y1 - y0) * st.scale);
        let too_small = match s.tool {
            Tool::Arrow => (w * w + h * h).sqrt() < 6.0,
            _ => w < 4.0 || h < 4.0,
        };
        if too_small {
            // A click, not a drag: discard it.
            if s.tool == Tool::Crop {
                invalidate(hwnd, st.view);
            } else {
                invalidate(hwnd, dirty);
            }
            return;
        }
    }
    if s.tool == Tool::Crop {
        do_crop(hwnd, st, &s);
        return;
    }
    apply_stroke(&st.committed, &s);
    st.strokes.push(s);
    committed_changed(hwnd, st);
}

// ---------------------------------------------------------------------------
// Text labels: a small text box appears where you click; Enter (or clicking away)
// places the text, Esc cancels.
// ---------------------------------------------------------------------------

fn begin_text(hwnd: HWND, st: &mut Editor, x: i32, y: i32) {
    let at = clamp_to_image(st, to_image_space(st, x, y));
    let h = sc(TEXT_PX, st.dpi) + sc(10, st.dpi);
    let w = (st.view.right - x).max(sc(160, st.dpi));
    let cls = WStr::new("EDIT");
    unsafe {
        let Ok(edit) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cls.p(),
            None,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            x,
            y - sc(4, st.dpi),
            w,
            h,
            Some(hwnd),
            None,
            Some(hinstance()),
            None,
        ) else {
            return;
        };
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(st.edit_font.0 as usize)), Some(LPARAM(1)));
        SendMessageW(edit, EM_SETMARGINS, Some(WPARAM(3)), Some(LPARAM(0))); // no inner margins
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, hwnd.0 as usize);
        let _ = SetFocus(Some(edit));
        st.entry = Some(TextEntry { edit, at, color: COLORS[st.color] });
    }
}

fn end_text(hwnd: HWND, st: &mut Editor, keep: bool) {
    let Some(entry) = st.entry.take() else { return };
    let text = unsafe {
        let n = GetWindowTextLengthW(entry.edit).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(entry.edit, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    };
    unsafe {
        let _ = DestroyWindow(entry.edit);
        let _ = SetFocus(Some(hwnd));
    }
    if keep && !text.trim().is_empty() {
        let s = Stroke {
            tool: Tool::Text,
            color: entry.color,
            width: stroke_width(Tool::Text, st.dpi) / st.scale,
            pts: vec![entry.at],
            text,
        };
        apply_stroke(&st.committed, &s);
        st.strokes.push(s);
        committed_changed(hwnd, st);
    }
    invalidate(hwnd, st.view);
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
            end_text(hwnd, st, true);
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
            end_text(hwnd, st, true);
            st.dpi = (hiword(wp.0) as u32).max(96);
            let _ = DeleteObject(st.font.into());
            st.font = ui_font(sc(13, st.dpi), 500);
            let _ = DeleteObject(st.edit_font.into());
            st.edit_font = label_font(sc(TEXT_PX, st.dpi));
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
            SetTextColor(hdc, rgb_from_argb(color));
            SetBkColor(hdc, rgb_from_argb(argb(255, 44, 44, 50)));
            LRESULT(st.edit_brush.0 as isize)
        }
        WM_APP_TEXT_DONE => {
            end_text(hwnd, st, wp.0 != 0);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if loword(lp.0 as usize) as u32 == HTCLIENT {
                let (x, y) = cursor_pos();
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                let c = if pt_in(&st.view, p.x, p.y) {
                    if st.tool == Tool::Text { IDC_IBEAM } else { IDC_CROSS }
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
                end_text(hwnd, st, true);
                return LRESULT(0);
            }
            let (x, y) = lparam_point(lp);
            if let Some((b, _)) = st.buttons.iter().copied().find(|(_, r)| pt_in(r, x, y)) {
                press(hwnd, st, b);
                return LRESULT(0);
            }
            if pt_in(&st.view, x, y) && st.current.is_none() {
                if st.tool == Tool::Text {
                    begin_text(hwnd, st, x, y);
                    return LRESULT(0);
                }
                let p = to_image_space(st, x, y);
                let p = if st.tool.two_point() { clamp_to_image(st, p) } else { p };
                let s = Stroke {
                    tool: st.tool,
                    color: COLORS[st.color],
                    // Same thickness on screen no matter how far the view is zoomed out.
                    width: stroke_width(st.tool, st.dpi) / st.scale,
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
            if st.current.is_some() {
                let raw = to_image_space(st, x, y);
                let two = st.current.as_ref().map_or(false, |s| s.tool.two_point());
                let p = if two { clamp_to_image(st, raw) } else { raw };
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
                    } else {
                        s.pts.push(p);
                        Some([last, p])
                    }
                };
                let s = st.current.as_ref().unwrap();
                let reach = s.reach();
                let r = if s.tool == Tool::Crop {
                    st.view // the shading covers the whole image
                } else {
                    match dirty {
                        // Free-hand: only the newest segment changed.
                        Some(seg) => screen_bounds(st, &seg, reach),
                        // Shapes: old and new outline.
                        None => {
                            let now = screen_bounds(st, &s.pts, reach);
                            let r = union(st.current_dirty, now);
                            st.current_dirty = now;
                            r
                        }
                    }
                };
                invalidate(hwnd, r);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            finish_stroke(hwnd, st);
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
            let tool = |t| Btn::Tool(t);
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                VK_Z if ctrl => undo(hwnd, st),
                VK_C if ctrl => copy_now(hwnd, st),
                VK_P if !ctrl => press(hwnd, st, tool(Tool::Pen)),
                VK_H | VK_M if !ctrl => press(hwnd, st, tool(Tool::Highlighter)),
                VK_A if !ctrl => press(hwnd, st, tool(Tool::Arrow)),
                VK_R if !ctrl => press(hwnd, st, tool(Tool::Rect)),
                VK_T if !ctrl => press(hwnd, st, tool(Tool::Text)),
                VK_B if !ctrl => press(hwnd, st, tool(Tool::Blur)),
                VK_C if !ctrl => press(hwnd, st, tool(Tool::Crop)),
                k if !ctrl && (VK_1.0..=VK_5.0).contains(&k.0) => {
                    press(hwnd, st, Btn::Color((k.0 - VK_1.0) as usize))
                }
                _ => {}
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
            end_text(hwnd, st, true);
            if st.current.is_some() {
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
