//! Scrolling capture: pick an area, then scroll; Marq keeps grabbing that area and
//! stitches the new rows onto the bottom, building one tall image.
//!
//! How it lines frames up: each row of pixels gets a hash. When the page has scrolled
//! by `dy` rows, row `r` of the new frame equals row `r + dy` of the previous one.
//! We try every `dy`, score how many (non-blank) rows agree, and take the best.
//!
//! The outline and the control bar are excluded from screen capture by Windows
//! (WDA_EXCLUDEFROMCAPTURE), so they never end up in the image.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::util::*;

const FRAME_CLASS: &str = "MarqScrollFrame";
const BAR_CLASS: &str = "MarqScrollBar";
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 90;
/// Stop growing past this height (keeps memory and the final PNG reasonable).
const MAX_HEIGHT: usize = 30_000;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);

// ---------------------------------------------------------------------------
// Stitching (pure, unit-tested)
// ---------------------------------------------------------------------------

/// A hash per row, and whether the row has any detail at all (blank rows match
/// everything, so they don't count as evidence).
pub fn row_hashes(px: &[u32], w: usize, h: usize) -> (Vec<u64>, Vec<bool>) {
    let mut hashes = Vec::with_capacity(h);
    let mut detail = Vec::with_capacity(h);
    for y in 0..h {
        let row = &px[y * w..(y + 1) * w];
        let mut hsh: u64 = 0xcbf2_9ce4_8422_2325;
        let first = row[0] & 0x00FF_FFFF;
        let mut varied = false;
        for &c in row.iter() {
            let c = c & 0x00FF_FFFF;
            varied |= c != first;
            hsh = (hsh ^ c as u64).wrapping_mul(0x0100_0000_01b3);
        }
        hashes.push(hsh);
        detail.push(varied);
    }
    (hashes, detail)
}

/// How many rows the content moved up between `prev` and `next` (0 = no change).
/// None if they don't overlap convincingly (scrolled too far, or the page changed).
pub fn find_offset(prev: &[u64], next: &[u64], next_detail: &[bool], last_dy: usize) -> Option<usize> {
    let h = prev.len();
    if h == 0 || next.len() != h {
        return None;
    }
    if prev == next {
        return Some(0);
    }
    // Ignore the top and bottom 10% of the new frame: sticky headers/footers live there.
    let top = h / 10;
    let bottom = h - h / 10;
    let min_rows = 8.max(h / 20);
    let mut best: Option<(f32, usize)> = None;
    for dy in 1..=(h * 9 / 10) {
        let end = bottom.min(h - dy);
        if end <= top {
            break;
        }
        let (mut agree, mut counted) = (0usize, 0usize);
        for r in top..end {
            if !next_detail[r] {
                continue;
            }
            counted += 1;
            if next[r] == prev[r + dy] {
                agree += 1;
            }
        }
        if counted < min_rows {
            continue;
        }
        let score = agree as f32 / counted as f32;
        if score < 0.75 {
            continue;
        }
        let better = match best {
            None => true,
            Some((s, d)) => {
                score > s + 0.02
                    || ((score - s).abs() <= 0.02 && dy.abs_diff(last_dy) < d.abs_diff(last_dy))
            }
        };
        if better {
            best = Some((score, dy));
        }
    }
    best.map(|(_, dy)| dy)
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    rect: RECT, // screen coordinates of the captured area
    w: usize,
    h: usize,
    stitched: Vec<u32>,
    rows: usize,
    last: Option<(Vec<u64>, Vec<bool>)>,
    last_dy: usize,
    frame: HWND,
    bar: HWND,
    dpi: u32,
    font: HFONT,
    font_b: HFONT,
    lost: bool, // couldn't line the last frame up
    hot: Option<usize>,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.font_b.into());
        }
    }
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

pub fn active() -> bool {
    SESSION.with(|s| s.borrow().is_some())
}

pub fn register() {
    register_class(&WStr::new(FRAME_CLASS), Some(frame_proc), IDC_ARROW, WNDCLASS_STYLES(0));
    register_class(&WStr::new(BAR_CLASS), Some(bar_proc), IDC_HAND, CS_DROPSHADOW);
}

/// Start a scrolling capture of `r` (screen coordinates).
pub fn start(r: RECT) {
    if active() || rect_w(&r) < 16 || rect_h(&r) < 32 {
        return;
    }
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let dpi = dpi_for_point(cx, cy);
    let work = work_area_for_point(cx, cy);
    let b = sc(3, dpi);

    // Outline just outside the area: a ring-shaped window, so scrolling inside reaches the page.
    let outer = rect(r.left - b, r.top - b, r.right + b, r.bottom + b);
    let frame = unsafe {
        let cls = WStr::new(FRAME_CLASS);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            cls.p(), None, WS_POPUP,
            outer.left, outer.top, rect_w(&outer), rect_h(&outer),
            None, None, Some(hinstance()), None,
        ) else {
            return;
        };
        let o = CreateRectRgn(0, 0, rect_w(&outer), rect_h(&outer));
        let i = CreateRectRgn(b, b, rect_w(&outer) - b, rect_h(&outer) - b);
        CombineRgn(Some(o), Some(o), Some(i), RGN_DIFF);
        let _ = DeleteObject(i.into());
        SetWindowRgn(hwnd, Some(o), false); // the window owns the region now
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        hwnd
    };

    // Control bar: below the area if there's room, else above, else inside the bottom edge.
    let (bw, bh) = (sc(400, dpi), sc(46, dpi));
    let bx = (cx - bw / 2).clamp(work.left, (work.right - bw).max(work.left));
    let below = r.bottom + b + sc(10, dpi);
    let above = r.top - b - sc(10, dpi) - bh;
    let by = if below + bh <= work.bottom {
        below
    } else if above >= work.top {
        above
    } else {
        r.bottom - bh - sc(12, dpi)
    };
    let bar = unsafe {
        let cls = WStr::new(BAR_CLASS);
        let title = WStr::new("Marq");
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            cls.p(), title.p(), WS_POPUP,
            bx, by, bw, bh,
            None, None, Some(hinstance()), None,
        ) else {
            let _ = DestroyWindow(frame);
            return;
        };
        modern_chrome(hwnd, true, true);
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        hwnd
    };

    let (w, h) = (rect_w(&r) as usize, rect_h(&r) as usize);
    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session {
            rect: r,
            w,
            h,
            stitched: Vec::new(),
            rows: 0,
            last: None,
            last_dy: 0,
            frame,
            bar,
            dpi,
            font: ui_font(sc(13, dpi), 400),
            font_b: ui_font(sc(13, dpi), 600),
            lost: false,
            hot: None,
        })
    });
    unsafe {
        let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(bar, SW_SHOWNOACTIVATE);
        // First frame on the first tick, once the snip overlay has left the screen.
        SetTimer(Some(bar), TIMER_TICK, TICK_MS, None);
    }
}

fn grab(r: RECT) -> Option<Vec<u32>> {
    let (w, h) = (rect_w(&r), rect_h(&r));
    let dib = Dib::new(w, h)?;
    unsafe {
        let screen = GetDC(None);
        let _ = BitBlt(dib.dc, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY);
        ReleaseDC(None, screen);
        let _ = GdiFlush();
    }
    let mut px = dib.pixels_mut().to_vec();
    for p in px.iter_mut() {
        *p |= 0xFF00_0000;
    }
    Some(px)
}

/// Grab the area and add whatever scrolled into view.
fn tick() {
    let Some(r) = SESSION.with(|s| s.borrow().as_ref().map(|s| s.rect)) else { return };
    if SESSION.with(|s| s.borrow().as_ref().map_or(true, |s| s.last.is_none())) {
        unsafe {
            let _ = DwmFlush();
        }
    }
    let Some(px) = grab(r) else { return };
    let mut full = false;
    let mut repaint = false;
    SESSION.with(|cell| {
        let mut b = cell.borrow_mut();
        let Some(s) = b.as_mut() else { return };
        let (hashes, detail) = row_hashes(&px, s.w, s.h);
        match &s.last {
            None => {
                s.stitched = px;
                s.rows = s.h;
                s.last = Some((hashes, detail));
                repaint = true;
            }
            Some((prev, _)) => match find_offset(prev, &hashes, &detail, s.last_dy) {
                Some(0) => {
                    if s.lost {
                        s.lost = false;
                        repaint = true;
                    }
                }
                Some(dy) => {
                    // Only the rows that just scrolled into view.
                    s.stitched.extend_from_slice(&px[(s.h - dy) * s.w..]);
                    s.rows += dy;
                    s.last_dy = dy;
                    s.last = Some((hashes, detail));
                    s.lost = false;
                    full = s.rows >= MAX_HEIGHT;
                    repaint = true;
                }
                None => {
                    if !s.lost {
                        s.lost = true;
                        repaint = true;
                    }
                }
            },
        }
        if repaint {
            unsafe {
                let _ = InvalidateRect(Some(s.bar), None, false);
            }
        }
    });
    if full {
        finish();
    }
}

fn close() -> Option<Session> {
    let s = SESSION.with(|s| s.borrow_mut().take())?;
    unsafe {
        let _ = KillTimer(Some(s.bar), TIMER_TICK);
        let _ = DestroyWindow(s.bar);
        let _ = DestroyWindow(s.frame);
    }
    Some(s)
}

/// Stop and hand the tall image to the app (clipboard, Gallery, preview card).
pub fn finish() {
    let Some(mut s) = close() else { return };
    if s.rows == 0 {
        // Finished before the first tick: just take what's there now.
        if let Some(px) = grab(s.rect) {
            s.stitched = px;
            s.rows = s.h;
        }
    }
    if s.rows == 0 {
        return;
    }
    let img = Image { w: s.w as i32, h: s.rows as i32, px: std::mem::take(&mut s.stitched) };
    let near = s.rect;
    drop(s);
    crate::on_snip(img, near);
    trim_memory();
}

pub fn cancel() {
    drop(close());
    trim_memory();
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let brush = CreateSolidBrush(rgb_from_argb(ACCENT));
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Done and Cancel button rectangles inside the bar.
fn bar_buttons(s: &Session, rc: RECT) -> [RECT; 2] {
    let (bw, bh) = (sc(74, s.dpi), sc(30, s.dpi));
    let y = (rect_h(&rc) - bh) / 2;
    let done = rect(rc.right - sc(8, s.dpi) - bw, y, rc.right - sc(8, s.dpi), y + bh);
    let cancel = rect(done.left - sc(6, s.dpi) - bw, y, done.left - sc(6, s.dpi), y + bh);
    [done, cancel]
}

fn paint_bar(hwnd: HWND, s: &Session) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        if let Some(back) = Dib::new(rc.right, rc.bottom) {
            let [done, cancel] = bar_buttons(s, rc);
            let dpi = s.dpi;
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.fill_round_rect(0.0, 0.0, rc.right as f32, rc.bottom as f32, 0.0, argb(255, 30, 30, 34));
                let dot = if s.lost { argb(255, 0xFF, 0x9F, 0x0A) } else { argb(255, 0xFF, 0x3B, 0x30) };
                g.fill_ellipse(scf(18.0, dpi), rc.bottom as f32 / 2.0, scf(4.5, dpi), dot);
                let c = if s.hot == Some(0) { argb(255, 0x9E, 0x86, 0xFF) } else { ACCENT };
                g.fill_round_rect(done.left as f32, done.top as f32, rect_w(&done) as f32, rect_h(&done) as f32, scf(8.0, dpi), c);
                let c = if s.hot == Some(1) { argb(255, 62, 62, 70) } else { argb(255, 48, 48, 54) };
                g.fill_round_rect(cancel.left as f32, cancel.top as f32, rect_w(&cancel) as f32, rect_h(&cancel) as f32, scf(8.0, dpi), c);
            }
            let msg = if s.lost {
                "Scroll back up a little \u{2014} too fast".to_string()
            } else if s.rows == 0 {
                "Scroll down to capture more".to_string()
            } else {
                format!("Scroll down \u{00B7} {} px tall", group(s.rows))
            };
            let tr = rect(sc(32, dpi), 0, cancel.left - sc(8, dpi), rc.bottom);
            draw_text(back.dc, &msg, tr, s.font, argb(255, 235, 235, 242), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
            let single = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
            draw_text(back.dc, "Done", done, s.font_b, argb(255, 255, 255, 255), single);
            draw_text(back.dc, "Cancel", cancel, s.font, argb(255, 225, 225, 232), single);
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back.dc), 0, 0, SRCCOPY);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 12345 -> "12,345"
fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let painted = SESSION.with(|c| {
                let b = c.borrow();
                b.as_ref().map(|s| paint_bar(hwnd, s)).is_some()
            });
            if !painted {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER if wp.0 == TIMER_TICK => {
            tick();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            SESSION.with(|c| {
                if let Some(s) = c.borrow_mut().as_mut() {
                    let hot = bar_buttons(s, rc).iter().position(|b| pt_in(b, x, y));
                    if hot != s.hot {
                        s.hot = hot;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let hit = SESSION.with(|c| c.borrow().as_ref().and_then(|s| bar_buttons(s, rc).iter().position(|b| pt_in(b, x, y))));
            match hit {
                Some(0) => finish(),
                Some(_) => cancel(),
                None => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake "page" of distinct rows, and a window of it scrolled to `top`.
    fn page(rows: usize, w: usize) -> Vec<u32> {
        let mut v = Vec::with_capacity(rows * w);
        for y in 0..rows {
            for x in 0..w {
                v.push(((y * 7919 + x * 31) % 0xFF_FFFF) as u32);
            }
        }
        v
    }
    fn window(p: &[u32], w: usize, top: usize, h: usize) -> Vec<u32> {
        p[top * w..(top + h) * w].to_vec()
    }

    #[test]
    fn finds_scroll_amount() {
        let (w, h) = (40, 200);
        let p = page(1000, w);
        let a = window(&p, w, 0, h);
        let (ha, _) = row_hashes(&a, w, h);
        for dy in [1usize, 17, 60, 150] {
            let b = window(&p, w, dy, h);
            let (hb, db) = row_hashes(&b, w, h);
            assert_eq!(find_offset(&ha, &hb, &db, 0), Some(dy), "dy={dy}");
        }
        // Same frame: no movement.
        let (hs, ds) = row_hashes(&a, w, h);
        assert_eq!(find_offset(&ha, &hs, &ds, 0), Some(0));
        // Scrolled past the whole frame: can't line up.
        let far = window(&p, w, 400, h);
        let (hf, df) = row_hashes(&far, w, h);
        assert_eq!(find_offset(&ha, &hf, &df, 0), None);
    }

    #[test]
    fn sticky_header_still_lines_up() {
        let (w, h) = (40, 200);
        let p = page(1000, w);
        let header: Vec<u32> = (0..w * 15).map(|i| 0x123456 ^ i as u32).collect();
        let with_header = |top: usize| {
            let mut f = window(&p, w, top, h);
            f[..w * 15].copy_from_slice(&header);
            f
        };
        let (ha, _) = row_hashes(&with_header(0), w, h);
        let (hb, db) = row_hashes(&with_header(40), w, h);
        assert_eq!(find_offset(&ha, &hb, &db, 0), Some(40));
    }

    #[test]
    fn stitches_back_the_page() {
        let (w, h) = (24, 120);
        let p = page(600, w);
        let mut stitched = window(&p, w, 0, h);
        let mut rows = h;
        let (mut last, _) = row_hashes(&stitched, w, h);
        let mut top = 0;
        for step in [10usize, 35, 0, 50, 20, 64] {
            top += step;
            let f = window(&p, w, top, h);
            let (hh, dd) = row_hashes(&f, w, h);
            let dy = find_offset(&last, &hh, &dd, 0).unwrap();
            assert_eq!(dy, step);
            if dy > 0 {
                stitched.extend_from_slice(&f[(h - dy) * w..]);
                rows += dy;
                last = hh;
            }
        }
        assert_eq!(rows, top + h);
        assert_eq!(stitched, window(&p, w, 0, top + h));
    }

    #[test]
    fn grouping() {
        assert_eq!(group(7), "7");
        assert_eq!(group(1240), "1,240");
        assert_eq!(group(30000), "30,000");
    }
}
