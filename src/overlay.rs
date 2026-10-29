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
    buttons: [RECT; 4],
    hot: Option<usize>,
    pressed: Option<usize>, // toolbar button under mouse-down
    armed: bool,            // window/screen click started on the overlay
    dpi: u32,
    font: HFONT,
    font_small: HFONT,
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
        buttons: [RECT::default(); 4],
        hot: None,
        pressed: None,
        armed: false,
        dpi,
        font,
        font_small,
    });
    layout_toolbar(&mut st, cur_mon);
    st.update_hover();

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
    let labels = ["Region", "Window", "Screen", ""];
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
            .map(|(i, l)| if i == 3 { sc(32, dpi) } else { text_width(dc, l, st.font) + pad * 2 })
            .collect();
        ReleaseDC(None, dc);
        ws
    };
    let total: i32 = widths.iter().sum::<i32>() + gap * 3 + inner * 2 + sc(8, dpi);
    let left = mon.left + (mon.right - mon.left - total) / 2;
    let top = mon.top + sc(18, dpi);
    st.toolbar = rect(left, top, left + total, top + h);
    let mut x = left + inner;
    for (i, w) in widths.iter().enumerate() {
        if i == 3 {
            x += sc(8, dpi); // small separator before close
        }
        st.buttons[i] = rect(x, top + inner, x + w, top + h - inner);
        x += w + gap;
    }
}

impl Overlay {
    fn selection(&self) -> Option<RECT> {
        match self.mode {
            Mode::Region => {
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
            Mode::Region => None,
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
    st.mode = mode;
    st.dragging = false;
    st.armed = false;
    st.update_hover();
    let new = st.selection();
    invalidate_change(hwnd, st, old, new);
    invalidate(hwnd, Some(st.toolbar));
}

/// Crop the chosen rectangle, close the overlay and hand the image to the app.
fn finish(hwnd: HWND, sel: RECT) {
    let result = unsafe { get_state::<Overlay>(hwnd) }.map(|st| {
        let img = crop(st.frozen.dib.pixels_mut(), st.frozen.dib.w, st.frozen.dib.h, sel);
        let (ox, oy) = st.frozen.origin;
        (img, rect(sel.left + ox, sel.top + oy, sel.right + ox, sel.bottom + oy))
    });
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    if let Some((img, screen_rect)) = result {
        if img.w > 0 && img.h > 0 {
            crate::on_snip(img, screen_rect);
        }
    }
}

fn cancel(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn paint(hwnd: HWND, st: &mut Overlay) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let rc = ps.rcPaint;
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
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

        // 3. Toolbar.
        let mut tmp = RECT::default();
        if IntersectRect(&mut tmp, &st.toolbar, &rc).as_bool() {
            let t = st.toolbar;
            let dpi = st.dpi;
            let active = match st.mode {
                Mode::Region => 0,
                Mode::Window => 1,
                Mode::Screen => 2,
            };
            if let Some(g) = Graphics::from_hdc(back) {
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
            let labels = ["Region", "Window", "Screen"];
            for (i, b) in st.buttons.iter().take(3).enumerate() {
                let c = if i == active { argb(255, 255, 255, 255) } else { argb(255, 200, 200, 210) };
                draw_text(back, labels[i], *b, st.font, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            // Close button: a drawn X (no font dependency).
            if let Some(g) = Graphics::from_hdc(back) {
                let b = st.buttons[3];
                let cx = (b.left + b.right) as f32 / 2.0;
                let cy = (b.top + b.bottom) as f32 / 2.0;
                let k = scf(5.0, dpi);
                let c = if st.hot == Some(3) { argb(255, 255, 255, 255) } else { argb(255, 200, 200, 210) };
                g.line(cx - k, cy - k, cx + k, cy + k, scf(1.6, dpi), c);
                g.line(cx - k, cy + k, cx + k, cy - k, scf(1.6, dpi), c);
            }
        }

        let _ = BitBlt(hdc, rc.left, rc.top, w, h, Some(back), rc.left, rc.top, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
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
            let (x, y) = st.cur;
            let c = if pt_in(&st.toolbar, x, y) {
                IDC_HAND
            } else if st.mode == Mode::Region {
                IDC_CROSS
            } else {
                IDC_HAND
            };
            SetCursor(LoadCursorW(None, c).ok());
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let old_sel = st.selection();
            st.cur = (x, y);

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
                Mode::Region => {
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
                        _ => cancel(hwnd),
                    }
                }
                return LRESULT(0);
            }
            if st.mode == Mode::Region && st.dragging {
                let _ = ReleaseCapture();
                let sel = st.selection().map(|s| st.clamp(s));
                let old = st.decor_bounds(st.selection());
                st.dragging = false;
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
            if !st.dragging {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
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
