//! Full-screen selection overlay over a frozen screenshot.
//! Modes: drag a region, click a window, or click a monitor.

use std::cell::Cell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::{self, Frozen};
use crate::gfx::{argb, Graphics};
use crate::util::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Region,
    Window,
    Screen,
    Text, // drag like Region, but copy the text inside instead of the picture
}

impl Mode {
    /// Modes where you drag out a box.
    fn drags(self) -> bool {
        matches!(self, Mode::Region | Mode::Text)
    }
}

const CLASS: &str = "MarqOverlay";
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);

thread_local! {
    static OPEN: Cell<bool> = const { Cell::new(false) };
}

struct Overlay {
    frozen: Frozen,
    dim: Dib,
    back: Dib,
    mode: Mode,
    windows: Vec<RECT>,  // client coords
    monitors: Vec<RECT>, // client coords
    dragging: bool,
    start: (i32, i32),
    cur: (i32, i32),
    hover: Option<RECT>,
    toolbar: RECT,
    buttons: [RECT; 6], // Region, Window, Screen, Text, Settings, Close
    hot: Option<usize>,
    pressed: Option<usize>, // toolbar button under mouse-down
    armed: bool,            // window/screen click started on the overlay
    dpi: u32,
    font: HFONT,
    font_small: HFONT,
    magnifier: bool,
    loupe_last: Option<RECT>,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.font_small.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_CROSS, WNDCLASS_STYLES(0));
}

/// Freeze the screen and open the overlay. Ignored if one is already open.
pub fn start(mode: Mode) {
    if OPEN.with(|o| o.get()) {
        return;
    }
    // Make sure our own preview card isn't in the frozen frame.
    if crate::toast::close_current() {
        unsafe {
            let _ = windows::Win32::Graphics::Dwm::DwmFlush();
        }
    }
    let Some(frozen) = capture::capture_screen() else { return };
    let (ox, oy) = frozen.origin;
    let (w, h) = (frozen.dib.w, frozen.dib.h);

    let Some(dim) = Dib::new(w, h) else { return };
    for (d, s) in dim.pixels_mut().iter_mut().zip(frozen.dib.pixels_mut().iter()) {
        // ~50% darker, keeps it fast and readable.
        *d = ((s >> 1) & 0x007F_7F7F) | 0xFF00_0000;
    }
    let Some(back) = Dib::new(w, h) else { return };

    let to_client = |r: RECT| rect(r.left - ox, r.top - oy, r.right - ox, r.bottom - oy);
    let windows: Vec<RECT> = capture::window_rects().into_iter().map(to_client).collect();
    let monitors: Vec<RECT> = capture::monitor_rects().into_iter().map(to_client).collect();

    // Toolbar sits at the top-center of the monitor the cursor is on.
    let (cx, cy) = cursor_pos();
    let dpi = dpi_for_point(cx, cy);
    let font = ui_font(sc(14, dpi), 500);
    let font_small = ui_font(sc(12, dpi), 600);
    let cur_mon = monitors
        .iter()
        .copied()
        .find(|m| pt_in(m, cx - ox, cy - oy))
        .unwrap_or(rect(0, 0, w, h));

    let mut st = Box::new(Overlay {
        frozen,
        dim,
        back,
        mode,
        windows,
        monitors,
        dragging: false,
        start: (0, 0),
        cur: (cx - ox, cy - oy),
        hover: None,
        toolbar: RECT::default(),
        buttons: [RECT::default(); 6],
        hot: None,
        pressed: None,
        armed: false,
        dpi,
        font,
        font_small,
        magnifier: crate::settings::magnifier(),
        loupe_last: None,
    });
    layout_toolbar(&mut st, cur_mon);
    st.update_hover();
    st.loupe_last = st.loupe_rect();

    unsafe {
        let cls = WStr::new(CLASS);
        let title = WStr::new("Marq");
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls.p(),
            title.p(),
            WS_POPUP,
            ox,
            oy,
            w,
            h,
            None,
            None,
            Some(hinstance()),
            None,
        ) else {
            return;
        };
        set_state(hwnd, st);
        OPEN.with(|o| o.set(true));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd); // paint synchronously: no blank frame
        force_foreground(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

fn layout_toolbar(st: &mut Overlay, mon: RECT) {
    let labels = ["Region", "Window", "Screen", "Text", "", ""];
    let dpi = st.dpi;
    let pad = sc(14, dpi);
    let gap = sc(4, dpi);
    let h = sc(40, dpi);
    let inner = sc(4, dpi);
    let widths: Vec<i32> = unsafe {
        let dc = GetDC(None);
        let ws = labels
            .iter()
            .enumerate()
            .map(|(i, l)| if i >= 4 { sc(32, dpi) } else { text_width(dc, l, st.font) + pad * 2 })
            .collect();
        ReleaseDC(None, dc);
        ws
    };
    let total: i32 = widths.iter().sum::<i32>() + gap * 5 + inner * 2 + sc(8, dpi);
    let left = mon.left + (mon.right - mon.left - total) / 2;
    let top = mon.top + sc(18, dpi);
    st.toolbar = rect(left, top, left + total, top + h);
    let mut x = left + inner;
    for (i, w) in widths.iter().enumerate() {
        if i == 4 {
            x += sc(8, dpi); // small separator before settings + close
        }
        st.buttons[i] = rect(x, top + inner, x + w, top + h - inner);
        x += w + gap;
    }
}

impl Overlay {
    fn selection(&self) -> Option<RECT> {
        match self.mode {
            Mode::Region | Mode::Text => {
                if self.dragging {
                    let (x0, y0) = self.start;
                    let (x1, y1) = self.cur;
                    Some(rect(x0.min(x1), y0.min(y1), x0.max(x1) + 1, y0.max(y1) + 1))
                } else {
                    None
                }
            }
            _ => self.hover,
        }
    }

    fn update_hover(&mut self) {
        let (x, y) = self.cur;
        self.hover = match self.mode {
            Mode::Region | Mode::Text => None,
            Mode::Window => self
                .windows
                .iter()
                .copied()
                .find(|r| pt_in(r, x, y))
                .or_else(|| self.monitors.iter().copied().find(|r| pt_in(r, x, y))),
            Mode::Screen => self.monitors.iter().copied().find(|r| pt_in(r, x, y)),
        }
        .map(|r| self.clamp(r));
    }

    /// Size of one magnified pixel and how many pixels across the loupe shows.
    fn loupe_cell(&self) -> (i32, i32) {
        (sc(8, self.dpi).max(6), 15)
    }

    /// Where the magnifier is drawn right now (loupe + coordinate strip), if shown.
    fn loupe_rect(&self) -> Option<RECT> {
        let (x, y) = self.cur;
        if !self.magnifier || !self.mode.drags() || pt_in(&self.toolbar, x, y) {
            return None;
        }
        let (cell, n) = self.loupe_cell();
        let size = cell * n;
        let fw = size;
        let fh = size + sc(24, self.dpi);
        let off = sc(22, self.dpi);
        let (w, h) = (self.frozen.dib.w, self.frozen.dib.h);
        let mut lx = x + off;
        if lx + fw > w {
            lx = x - off - fw;
        }
        let mut ly = y + off;
        if ly + fh > h {
            ly = y - off - fh;
        }
        let lx = lx.clamp(0, (w - fw).max(0));
        let ly = ly.clamp(0, (h - fh).max(0));
        Some(rect(lx, ly, lx + fw, ly + fh))
    }

    fn clamp(&self, r: RECT) -> RECT {
        rect(
            r.left.max(0),
            r.top.max(0),
            r.right.min(self.frozen.dib.w),
            r.bottom.min(self.frozen.dib.h),
        )
    }

    fn label_rect(&self, sel: RECT) -> RECT {
        let dpi = self.dpi;
        let w = sc(92, dpi);
        let h = sc(24, dpi);
        let gap = sc(8, dpi);
        let mut top = sel.bottom + gap;
        if top + h > self.frozen.dib.h {
            top = sel.top - gap - h;
        }
        if top < 0 {
            top = sel.top + gap;
        }
        let left = sel.left.clamp(0, (self.frozen.dib.w - w).max(0));
        rect(left, top, left + w, top + h)
    }

    /// Area that changes when a given selection is drawn (border + size label).
    fn decor_bounds(&self, sel: Option<RECT>) -> Option<RECT> {
        let sel = sel?;
        let p = sc(4, self.dpi);
        let mut b = rect(sel.left - p, sel.top - p, sel.right + p, sel.bottom + p);
        let l = self.label_rect(sel);
        let mut out = RECT::default();
        unsafe {
            let _ = UnionRect(&mut out, &b, &l);
        }
        b = out;
        Some(b)
    }
}

fn invalidate(hwnd: HWND, r: Option<RECT>) {
    if let Some(r) = r {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), Some(&r), false);
        }
    }
}

/// Repaint only what changed between two selections: the area that switched between
/// dim and bright (XOR), both borders, and both size labels. Keeps big drags smooth.
fn invalidate_change(hwnd: HWND, st: &Overlay, old: Option<RECT>, new: Option<RECT>) {
    match (old, new) {
        (Some(a), Some(b)) => unsafe {
            let ra = CreateRectRgn(a.left, a.top, a.right, a.bottom);
            let rb = CreateRectRgn(b.left, b.top, b.right, b.bottom);
            let rx = CreateRectRgn(0, 0, 0, 0);
            CombineRgn(Some(rx), Some(ra), Some(rb), RGN_XOR);
            let _ = InvalidateRgn(hwnd, Some(rx), false);
            let _ = DeleteObject(ra.into());
            let _ = DeleteObject(rb.into());
            let _ = DeleteObject(rx.into());
            let p = sc(4, st.dpi);
            for r in [a, b] {
                for strip in [
                    rect(r.left - p, r.top - p, r.right + p, r.top + p),
                    rect(r.left - p, r.bottom - p, r.right + p, r.bottom + p),
                    rect(r.left - p, r.top - p, r.left + p, r.bottom + p),
                    rect(r.right - p, r.top - p, r.right + p, r.bottom + p),
                ] {
                    invalidate(hwnd, Some(strip));
                }
                invalidate(hwnd, Some(st.label_rect(r)));
            }
        },
        (a, b) => {
            invalidate(hwnd, st.decor_bounds(a));
            invalidate(hwnd, st.decor_bounds(b));
        }
    }
}

fn set_mode(hwnd: HWND, st: &mut Overlay, mode: Mode) {
    if st.mode == mode {
        return;
    }
    let old = st.selection();
    if st.dragging {
        st.dragging = false;
        unsafe {
            let _ = ReleaseCapture();
        }
    }
    st.mode = mode;
    st.armed = false;
    st.update_hover();
    let new = st.selection();
    invalidate_change(hwnd, st, old, new);
    invalidate(hwnd, Some(st.toolbar));
    refresh_loupe(hwnd, st);
    update_cursor(st);
}

fn refresh_loupe(hwnd: HWND, st: &mut Overlay) {
    let now = st.loupe_rect();
    if now != st.loupe_last {
        invalidate(hwnd, st.loupe_last);
        invalidate(hwnd, now);
    } else if now.is_some() {
        invalidate(hwnd, now); // same spot, different pixels underneath
    }
    st.loupe_last = now;
}

/// Crop the chosen rectangle, close the overlay and hand the image to the app.
fn finish(hwnd: HWND, sel: RECT) {
    let result = unsafe { get_state::<Overlay>(hwnd) }.map(|st| {
        let img = crop(st.frozen.dib.pixels_mut(), st.frozen.dib.w, st.frozen.dib.h, sel);
        let (ox, oy) = st.frozen.origin;
        (img, rect(sel.left + ox, sel.top + oy, sel.right + ox, sel.bottom + oy), st.mode)
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    if let Some((img, screen_rect, mode)) = result {
        if img.w > 0 && img.h > 0 {
            if mode == Mode::Text {
                crate::on_text(img, screen_rect);
            } else {
                crate::on_snip(img, screen_rect);
            }
        }
    }
}

fn cancel(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Paint each rectangle of the update region on its own. BeginPaint only reports the
/// bounding box, which during a drag is most of the screen even though only thin
/// strips changed.
fn paint(hwnd: HWND, st: &mut Overlay) {
    unsafe {
        let mut rects: Vec<RECT> = Vec::new();
        let rgn = CreateRectRgn(0, 0, 0, 0);
        if GetUpdateRgn(hwnd, rgn, false) == COMPLEXREGION {
            let size = GetRegionData(rgn, 0, None);
            if size as usize > std::mem::size_of::<RGNDATAHEADER>() {
                let mut buf = vec![0u8; size as usize];
                let rd = buf.as_mut_ptr() as *mut RGNDATA;
                if GetRegionData(rgn, size, Some(rd)) != 0 {
                    let hdr = &(*rd).rdh;
                    let first = buf.as_ptr().add(hdr.dwSize as usize) as *const RECT;
                    let n = hdr.nCount as usize;
                    if n <= 64 {
                        rects.extend_from_slice(std::slice::from_raw_parts(first, n));
                    }
                }
            }
        }
        let _ = DeleteObject(rgn.into());

        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if rects.is_empty() {
            rects.push(ps.rcPaint);
        }
        for rc in rects {
            compose(st, rc);
            let _ = BitBlt(hdc, rc.left, rc.top, rect_w(&rc), rect_h(&rc), Some(st.back.dc), rc.left, rc.top, SRCCOPY);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Render one rectangle of the overlay into the back buffer.
fn compose(st: &Overlay, rc: RECT) {
    unsafe {
        let (w, h) = (rect_w(&rc), rect_h(&rc));
        let back = st.back.dc;

        // 1. Dimmed screen.
        let _ = BitBlt(back, rc.left, rc.top, w, h, Some(st.dim.dc), rc.left, rc.top, SRCCOPY);

        // 2. Selection shown at full brightness with an accent border and size label.
        if let Some(sel) = st.selection() {
            let mut i = RECT::default();
            if IntersectRect(&mut i, &sel, &rc).as_bool() {
                let _ = BitBlt(
                    back,
                    i.left,
                    i.top,
                    i.right - i.left,
                    i.bottom - i.top,
                    Some(st.frozen.dib.dc),
                    i.left,
                    i.top,
                    SRCCOPY,
                );
            }
            let lbl = st.label_rect(sel);
            if let Some(g) = Graphics::from_hdc(back) {
                g.clip(rc.left, rc.top, w, h);
                let bw = scf(2.0, st.dpi);
                g.stroke_round_rect(
                    sel.left as f32 - bw / 2.0,
                    sel.top as f32 - bw / 2.0,
                    (sel.right - sel.left) as f32 + bw,
                    (sel.bottom - sel.top) as f32 + bw,
                    0.0,
                    bw,
                    ACCENT,
                );
                g.fill_round_rect(
                    lbl.left as f32,
                    lbl.top as f32,
                    (lbl.right - lbl.left) as f32,
                    (lbl.bottom - lbl.top) as f32,
                    scf(6.0, st.dpi),
                    argb(230, 24, 24, 28),
                );
            }
            let txt = format!("{} \u{00D7} {}", sel.right - sel.left, sel.bottom - sel.top);
            draw_text(back, &txt, lbl, st.font_small, argb(255, 240, 240, 245), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }

        // 3. Magnifier.
        if let Some(lr) = st.loupe_rect() {
            if intersect(lr, rc).is_some() {
                draw_loupe(st, lr, rc);
            }
        }

        // 4. Toolbar.
        let mut tmp = RECT::default();
        if IntersectRect(&mut tmp, &st.toolbar, &rc).as_bool() {
            let t = st.toolbar;
            let dpi = st.dpi;
            let active = match st.mode {
                Mode::Region => 0,
                Mode::Window => 1,
                Mode::Screen => 2,
                Mode::Text => 3,
            };
            if let Some(g) = Graphics::from_hdc(back) {
                g.clip(rc.left, rc.top, w, h);
                let r = (t.bottom - t.top) as f32 / 2.0;
                g.fill_round_rect(
                    t.left as f32,
                    t.top as f32,
                    (t.right - t.left) as f32,
                    (t.bottom - t.top) as f32,
                    r,
                    argb(240, 28, 28, 32),
                );
                g.stroke_round_rect(
                    t.left as f32 + 0.5,
                    t.top as f32 + 0.5,
                    (t.right - t.left) as f32 - 1.0,
                    (t.bottom - t.top) as f32 - 1.0,
                    r,
                    1.0,
                    argb(60, 255, 255, 255),
                );
                for (i, b) in st.buttons.iter().enumerate() {
                    let br = (b.bottom - b.top) as f32 / 2.0;
                    let fill = if i == active {
                        Some(ACCENT)
                    } else if st.hot == Some(i) {
                        Some(argb(40, 255, 255, 255))
                    } else {
                        None
                    };
                    if let Some(c) = fill {
                        g.fill_round_rect(
                            b.left as f32,
                            b.top as f32,
                            (b.right - b.left) as f32,
                            (b.bottom - b.top) as f32,
                            br,
                            c,
                        );
                    }
                }
            }
            let labels = ["Region", "Window", "Screen", "Text"];
            for (i, b) in st.buttons.iter().take(4).enumerate() {
                let c = if i == active { argb(255, 255, 255, 255) } else { argb(255, 200, 200, 210) };
                draw_text(back, labels[i], *b, st.font, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            // Settings gear and close X: drawn shapes, no font dependency.
            if let Some(g) = Graphics::from_hdc(back) {
                g.clip(rc.left, rc.top, w, h);
                let icon = |i: usize| if st.hot == Some(i) { argb(255, 255, 255, 255) } else { argb(255, 200, 200, 210) };

                let b = st.buttons[4];
                let cx = (b.left + b.right) as f32 / 2.0;
                let cy = (b.top + b.bottom) as f32 / 2.0;
                let c = icon(4);
                let ring = scf(4.6, dpi);
                g.stroke_ellipse(cx, cy, ring, scf(2.4, dpi), c);
                for k in 0..8 {
                    let a = k as f32 * std::f32::consts::FRAC_PI_4;
                    let (sx, sy) = (a.cos(), a.sin());
                    g.line(
                        cx + sx * ring, cy + sy * ring,
                        cx + sx * scf(7.4, dpi), cy + sy * scf(7.4, dpi),
                        scf(2.6, dpi), c,
                    );
                }

                let b = st.buttons[5];
                let cx = (b.left + b.right) as f32 / 2.0;
                let cy = (b.top + b.bottom) as f32 / 2.0;
                let k = scf(5.0, dpi);
                let c = icon(5);
                g.line(cx - k, cy - k, cx + k, cy + k, scf(1.6, dpi), c);
                g.line(cx - k, cy + k, cx + k, cy - k, scf(1.6, dpi), c);
            }
        }

    }
}

/// Pixel-level zoom of the area under the cursor, with its screen coordinates.
fn draw_loupe(st: &Overlay, lr: RECT, rc: RECT) {
    let (cell, n) = st.loupe_cell();
    let size = cell * n;
    let (cx, cy) = st.cur;
    let (w, h) = (st.frozen.dib.w, st.frozen.dib.h);
    let half = n / 2;
    let (sx0, sy0) = (cx - half, cy - half);
    let dpi = st.dpi;
    unsafe {
        let back = st.back.dc;
        if let Some(g) = Graphics::from_hdc(back) {
            g.clip(rc.left, rc.top, rect_w(&rc), rect_h(&rc));
            g.fill_round_rect(lr.left as f32, lr.top as f32, rect_w(&lr) as f32, rect_h(&lr) as f32, scf(6.0, dpi), argb(245, 22, 22, 26));
        }
        // Only the part of the 15x15 source that's actually on screen.
        let ix0 = sx0.max(0);
        let iy0 = sy0.max(0);
        let ix1 = (sx0 + n).min(w);
        let iy1 = (sy0 + n).min(h);
        if ix1 > ix0 && iy1 > iy0 {
            let old = SetStretchBltMode(back, COLORONCOLOR); // crisp pixels, no smoothing
            let _ = StretchBlt(
                back,
                lr.left + (ix0 - sx0) * cell,
                lr.top + (iy0 - sy0) * cell,
                (ix1 - ix0) * cell,
                (iy1 - iy0) * cell,
                Some(st.frozen.dib.dc),
                ix0,
                iy0,
                ix1 - ix0,
                iy1 - iy0,
                SRCCOPY,
            );
            SetStretchBltMode(back, STRETCH_BLT_MODE(old));
        }
        if let Some(g) = Graphics::from_hdc(back) {
            g.clip(rc.left, rc.top, rect_w(&rc), rect_h(&rc));
            // Center pixel marker: dark + light outline so it shows on any color.
            let px = (lr.left + half * cell) as f32;
            let py = (lr.top + half * cell) as f32;
            g.stroke_round_rect(px - 1.0, py - 1.0, cell as f32 + 2.0, cell as f32 + 2.0, 0.0, 1.0, argb(200, 0, 0, 0));
            g.stroke_round_rect(px, py, cell as f32, cell as f32, 0.0, 1.0, argb(255, 255, 255, 255));
            g.stroke_round_rect(
                lr.left as f32 + 0.5, lr.top as f32 + 0.5, rect_w(&lr) as f32 - 1.0, rect_h(&lr) as f32 - 1.0,
                scf(6.0, dpi), 1.0, argb(90, 255, 255, 255),
            );
        }
        let (ox, oy) = st.frozen.origin;
        let color = if cx >= 0 && cy >= 0 && cx < w && cy < h {
            st.frozen.dib.pixels_mut()[(cy * w + cx) as usize] & 0x00FF_FFFF
        } else {
            0
        };
        let txt = format!("{}, {}   #{:06X}", cx + ox, cy + oy, color);
        draw_text(
            back, &txt,
            rect(lr.left, lr.top + size, lr.right, lr.bottom),
            st.font_small, argb(255, 225, 225, 232), DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn update_cursor(st: &Overlay) {
    let (x, y) = st.cur;
    let c = if pt_in(&st.toolbar, x, y) {
        IDC_HAND
    } else if st.mode.drags() {
        IDC_CROSS
    } else {
        IDC_HAND
    };
    unsafe {
        SetCursor(LoadCursorW(None, c).ok());
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = get_state::<Overlay>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            // WM_SETCURSOR arrives before WM_MOUSEMOVE, so read the live position.
            let (x, y) = cursor_pos();
            let (ox, oy) = st.frozen.origin;
            st.cur = (x - ox, y - oy);
            update_cursor(st);
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let old_sel = st.selection();
            st.cur = (x, y);
            if st.dragging && (wp.0 & 0x0001) == 0 /* MK_LBUTTON */ {
                // Button released somewhere we didn't hear about: end the drag cleanly.
                st.dragging = false;
                let _ = ReleaseCapture();
            }

            let hot = st.buttons.iter().position(|b| pt_in(b, x, y));
            if hot != st.hot {
                st.hot = hot;
                invalidate(hwnd, Some(st.toolbar));
            }
            if !st.dragging {
                st.update_hover();
            }
            let new_sel = st.selection();
            if new_sel != old_sel {
                invalidate_change(hwnd, st, old_sel, new_sel);
            }
            if st.magnifier {
                refresh_loupe(hwnd, st);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_point(lp);
            st.pressed = None;
            st.armed = false;
            if pt_in(&st.toolbar, x, y) {
                st.pressed = st.buttons.iter().position(|b| pt_in(b, x, y));
                return LRESULT(0);
            }
            match st.mode {
                Mode::Region | Mode::Text => {
                    st.dragging = true;
                    st.start = (x, y);
                    st.cur = (x, y);
                    SetCapture(hwnd);
                    invalidate(hwnd, st.decor_bounds(st.selection()));
                }
                // Act on mouse-up so the app underneath never sees a stray button-up.
                _ => st.armed = true,
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lp);
            if let Some(i) = st.pressed.take() {
                if pt_in(&st.buttons[i], x, y) {
                    match i {
                        0 => set_mode(hwnd, st, Mode::Region),
                        1 => set_mode(hwnd, st, Mode::Window),
                        2 => set_mode(hwnd, st, Mode::Screen),
                        3 => set_mode(hwnd, st, Mode::Text),
                        4 => {
                            cancel(hwnd);
                            crate::settings_ui::open();
                        }
                        _ => cancel(hwnd),
                    }
                }
                return LRESULT(0);
            }
            if st.mode.drags() && st.dragging {
                // Read the selection and end the drag BEFORE releasing capture:
                // ReleaseCapture synchronously sends WM_CAPTURECHANGED, which would
                // otherwise treat this as a lost drag and throw the selection away.
                let sel = st.selection().map(|s| st.clamp(s));
                let old = st.decor_bounds(st.selection());
                st.dragging = false;
                let _ = ReleaseCapture();
                match sel {
                    Some(s) if rect_w(&s) >= 4 && rect_h(&s) >= 4 => finish(hwnd, s),
                    _ => invalidate(hwnd, old),
                }
            } else if st.armed {
                st.armed = false;
                if let Some(sel) = st.hover {
                    finish(hwnd, sel);
                }
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // Lost the mouse mid-drag (rare): drop the drag instead of getting stuck.
            if st.dragging && HWND(lp.0 as *mut _) != hwnd {
                let old = st.decor_bounds(st.selection());
                st.dragging = false;
                invalidate(hwnd, old);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            cancel(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = VIRTUAL_KEY(wp.0 as u16);
            match key {
                VK_ESCAPE => cancel(hwnd),
                VK_1 | VK_R => set_mode(hwnd, st, Mode::Region),
                VK_2 | VK_W => set_mode(hwnd, st, Mode::Window),
                VK_3 | VK_S | VK_F => set_mode(hwnd, st, Mode::Screen),
                VK_4 | VK_T => set_mode(hwnd, st, Mode::Text),
                VK_RETURN | VK_SPACE => {
                    if let Some(sel) = st.hover {
                        finish(hwnd, sel);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            // Don't leave a frozen screen hanging if the user alt-tabs away.
            // Posted (not immediate) so it's harmless while we're already closing.
            let _ = st;
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(take_state::<Overlay>(hwnd));
            OPEN.with(|o| o.set(false));
            trim_memory();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
