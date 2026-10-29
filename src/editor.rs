//! Minimal markup editor: pen, highlighter, arrow, five colors, undo.
//! Every finished stroke re-copies the image to the clipboard automatically.
//!
//! Smoothness: the image is pre-scaled once into a display buffer, the stroke in
//! progress is drawn at paint time (never baked into a full-size copy), and each
//! mouse move only repaints the few pixels the newest segment touches.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::util::*;

const CLASS: &str = "MarqEditor";
const TIMER_STATUS: usize = 1;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const BG: u32 = argb(255, 24, 24, 27);
const BAR: u32 = argb(255, 32, 32, 36);
const BAR_H: i32 = 52;
const MARGIN: i32 = 12;

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
    width: f32,
    pts: Vec<(f32, f32)>,
}

impl Stroke {
    /// How far ink can reach beyond the points (arrowheads are wider than the shaft).
    fn reach(&self) -> f32 {
        match self.tool {
            Tool::Arrow => self.width * 3.5 + 2.0,
            _ => self.width / 2.0 + 2.0,
        }
    }
}

struct Editor {
    base: Dib,              // original snip (for undo)
    committed: Dib,         // base + finished strokes, full resolution
    display: Option<Dib>,   // committed scaled to the view (None when scale == 1)
    back: Option<Dib>,      // persistent client-sized back buffer
    strokes: Vec<Stroke>,
    current: Option<Stroke>,
    current_dirty: RECT,    // screen-space bounds the current stroke has painted
    tool: Tool,
    color: usize,
    dpi: u32,
    font: HFONT,
    view: RECT,
    scale: f32,
    buttons: Vec<(Btn, RECT)>,
    hot: Option<usize>,
    copied: bool,
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_ARROW, WNDCLASS_STYLES(0));
}

pub fn open(img: Image) {
    let Some(base) = Dib::from_image(&img) else { return };
    let Some(committed) = Dib::from_image(&img) else { return };
    drop(img);

    let (cx, cy) = cursor_pos();
    let dpi = dpi_for_point(cx, cy);
    let work_area = work_area_for_point(cx, cy);

    // Size the window so the image fits in ~85% of the screen, never upscaled.
    let bar = sc(BAR_H, dpi);
    let margin = sc(MARGIN, dpi);
    let max_w = (rect_w(&work_area) as f32 * 0.85) as i32;
    let max_h = (rect_h(&work_area) as f32 * 0.85) as i32;
    let s = ((max_w - margin * 2) as f32 / base.w as f32)
        .min((max_h - bar - margin * 2) as f32 / base.h as f32)
        .clamp(0.05, 1.0);
    let cw = ((base.w as f32 * s) as i32 + margin * 2).max(sc(600, dpi));
    let ch = ((base.h as f32 * s) as i32 + bar + margin * 2).max(sc(240, dpi));

    let st = Box::new(Editor {
        base,
        committed,
        display: None,
        back: None,
        strokes: Vec::new(),
        current: None,
        current_dirty: RECT::default(),
        tool: Tool::Pen,
        color: 0,
        dpi,
        font: ui_font(sc(13, dpi), 500),
        view: RECT::default(),
        scale: 1.0,
        buttons: Vec::new(),
        hot: None,
        copied: false,
    });

    unsafe {
        let style = WS_OVERLAPPEDWINDOW;
        let mut wr = rect(0, 0, cw, ch);
        let _ = AdjustWindowRectExForDpi(&mut wr, style, false, WINDOW_EX_STYLE(0), dpi);
        let ww = rect_w(&wr);
        let wh = rect_h(&wr);
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
        Tool::Pen => 4.0,
        Tool::Highlighter => 22.0,
        Tool::Arrow => 5.0,
    };
    scf(base, dpi)
}

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

    // Toolbar buttons.
    st.buttons.clear();
    let bh = sc(32, dpi);
    let y = (bar - bh) / 2;
    let mut x = sc(10, dpi);
    let gap = sc(4, dpi);
    let pad = sc(14, dpi);
    for (t, label) in [(Tool::Pen, "Pen"), (Tool::Highlighter, "Highlighter"), (Tool::Arrow, "Arrow")] {
        let w = text_w(st.font, label) + pad * 2;
        st.buttons.push((Btn::Tool(t), rect(x, y, x + w, y + bh)));
        x += w + gap;
    }
    x += sc(12, dpi);
    for i in 0..COLORS.len() {
        st.buttons.push((Btn::Color(i), rect(x, y, x + bh, y + bh)));
        x += bh + gap;
    }
    x += sc(12, dpi);
    let undo_w = text_w(st.font, "Undo") + pad * 2;
    st.buttons.push((Btn::Undo, rect(x, y, x + undo_w, y + bh)));
    x += undo_w + gap;
    let copy_w = text_w(st.font, "Copy").max(text_w(st.font, "Copied \u{2713}")) + pad * 2;
    st.buttons.push((Btn::Copy, rect(x, y, x + copy_w, y + bh)));
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
        Btn::Tool(Tool::Pen) => "Pen",
        Btn::Tool(Tool::Highlighter) => "Highlighter",
        Btn::Tool(Tool::Arrow) => "Arrow",
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
    st.committed.copy_from(&st.base);
    if let Some(g) = Graphics::from_hdc(st.committed.dc) {
        for s in &st.strokes {
            draw_stroke(&g, s);
        }
    }
}

fn copy_now(hwnd: HWND, st: &mut Editor) {
    unsafe {
        let _ = GdiFlush();
    }
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

/// Committed changed: refresh the scaled copy, repaint the image, re-copy to clipboard.
fn committed_changed(hwnd: HWND, st: &mut Editor) {
    unsafe {
        let _ = GdiFlush();
    }
    rebuild_display(st);
    invalidate(hwnd, st.view);
    copy_now(hwnd, st);
}

fn undo(hwnd: HWND, st: &mut Editor) {
    if st.current.is_some() {
        return;
    }
    if st.strokes.pop().is_some() {
        rebuild_committed(st);
        committed_changed(hwnd, st);
    }
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
                    draw_stroke(&g, s);
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
    if let Some(g) = Graphics::from_hdc(st.committed.dc) {
        draw_stroke(&g, &s);
    }
    st.strokes.push(s);
    st.current_dirty = RECT::default();
    committed_changed(hwnd, st);
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
            layout(hwnd, st);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: sc(620, st.dpi), y: sc(260, st.dpi) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            st.dpi = (hiword(wp.0) as u32).max(96);
            let _ = DeleteObject(st.font.into());
            st.font = ui_font(sc(13, st.dpi), 500);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd, None, r.left, r.top, rect_w(r), rect_h(r),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            layout(hwnd, st);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if loword(lp.0 as usize) as u32 == HTCLIENT {
                let (x, y) = cursor_pos();
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                let c = if pt_in(&st.view, p.x, p.y) {
                    IDC_CROSS
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
            let (x, y) = lparam_point(lp);
            if let Some((b, _)) = st.buttons.iter().copied().find(|(_, r)| pt_in(r, x, y)) {
                press(hwnd, st, b);
                return LRESULT(0);
            }
            if pt_in(&st.view, x, y) && st.current.is_none() {
                let p = to_image_space(st, x, y);
                let s = Stroke {
                    tool: st.tool,
                    color: COLORS[st.color],
                    width: stroke_width(st.tool, st.dpi),
                    pts: vec![p, p],
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
                let p = to_image_space(st, x, y);
                let dirty = {
                    let s = st.current.as_mut().unwrap();
                    let last = *s.pts.last().unwrap();
                    if (last.0 - p.0).abs() < 0.5 && (last.1 - p.1).abs() < 0.5 {
                        return LRESULT(0); // sub-pixel jitter
                    }
                    if s.tool == Tool::Arrow {
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
                let r = match dirty {
                    // Free-hand: only the newest segment changed.
                    Some(seg) => screen_bounds(st, &seg, reach),
                    // Arrow: old and new arrow.
                    None => {
                        let now = screen_bounds(st, &s.pts, reach);
                        let r = union(st.current_dirty, now);
                        st.current_dirty = now;
                        r
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
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                VK_Z if ctrl => undo(hwnd, st),
                VK_C if ctrl => copy_now(hwnd, st),
                VK_P if !ctrl => press(hwnd, st, Btn::Tool(Tool::Pen)),
                VK_H if !ctrl => press(hwnd, st, Btn::Tool(Tool::Highlighter)),
                VK_A if !ctrl => press(hwnd, st, Btn::Tool(Tool::Arrow)),
                k if !ctrl && (VK_1.0..=VK_5.0).contains(&k.0) => {
                    press(hwnd, st, Btn::Color((k.0 - VK_1.0) as usize))
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_STATUS {
                let _ = KillTimer(Some(hwnd), TIMER_STATUS);
                st.copied = false;
                invalidate(hwnd, bar_rect(hwnd, st));
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = KillTimer(Some(hwnd), TIMER_STATUS);
            drop(take_state::<Editor>(hwnd));
            trim_memory();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
