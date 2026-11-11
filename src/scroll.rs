//! Scrolling capture: pick an area and Marq scrolls it for you, grabbing the area as
//! it goes and stitching the newly revealed rows onto the bottom, building one tall
//! image. It stops by itself at the end of the page. You can also scroll by hand.
//!
//! Frames are lined up by `stitch::find_offset`, which tolerates fractional-pixel
//! scrolling, anti-aliasing noise and sticky headers/footers (see stitch.rs).
//!
//! The outline and the control bar are excluded from screen capture by Windows
//! (WDA_EXCLUDEFROMCAPTURE), so they never end up in the image.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::stitch::{find_offset, signature, still_bottom, Sig};
use crate::util::*;

const FRAME_CLASS: &str = "MarqScrollFrame";
const BAR_CLASS: &str = "MarqScrollBar";
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 90;
/// Stop growing past this height (keeps memory and the final PNG reasonable).
const MAX_HEIGHT: usize = 30_000;
/// ...and past this many pixels (~160 MB), whichever comes first.
const MAX_PIXELS: usize = 40_000_000;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const WM_MOUSELEAVE_: u32 = 0x02A3;
/// From the keyboard hook: Esc was pressed during the capture.
const WM_APP_ESC: u32 = WM_APP + 40;
/// Auto-scroll: wait at least this many ticks after scrolling before judging the result...
const SETTLE_TICKS: u32 = 3;
/// ...and stop waiting for the page to sit still after this many (animated pages).
const MAX_SETTLE_TICKS: u32 = 14;
/// Wheel amount per step (120 = one notch). Adapted so each step moves ~45% of the area.
const STEP_START: i32 = 120;
const STEP_MIN: i32 = 20;
const STEP_MAX: i32 = 1200;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    rect: RECT, // screen coordinates of the captured area
    w: usize,
    h: usize,
    stitched: Vec<u32>,
    rows: usize,
    last: Option<Sig>,
    last_dy: usize,
    frame: HWND,
    bar: HWND,
    dpi: u32,
    font: HFONT,
    font_b: HFONT,
    status: Status,
    hot: Option<usize>,
    ticks: u32,
    /// Rows fixed at the bottom of the area (footer, status bar, sheet tabs) that
    /// stay put while the page scrolls. Kept aside and added once at the end.
    footer: Vec<u32>,
    footer_rows: usize,
    // Auto-scroll.
    step: i32,            // wheel amount per step
    since_send: u32,      // ticks since the last step
    moved_since_send: usize,
    sent_once: bool,
    stuck: u32,           // steps in a row that moved nothing (end of the page)
    paused: bool,         // pointer left the area: the user wants control
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    WentUp,
    Lost,
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
    // No CS_DROPSHADOW: the shadow is a separate window that capture exclusion may miss.
    register_class(&WStr::new(BAR_CLASS), Some(bar_proc), IDC_HAND, WNDCLASS_STYLES(0));
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

    // Control bar: somewhere outside the area if possible (below, above, right, left),
    // otherwise just inside its bottom edge (it's excluded from capture there).
    let (bw, bh) = (sc(470, dpi), sc(46, dpi));
    let gap = b + sc(10, dpi);
    let hx = (cx - bw / 2).clamp(work.left, (work.right - bw).max(work.left));
    let vy = (cy - bh / 2).clamp(work.top, (work.bottom - bh).max(work.top));
    let candidates = [
        (hx, r.bottom + gap),
        (hx, r.top - gap - bh),
        (r.right + gap, vy),
        (r.left - gap - bw, vy),
    ];
    let fits = |&(x, y): &(i32, i32)| {
        let c = rect(x, y, x + bw, y + bh);
        c.left >= work.left && c.top >= work.top && c.right <= work.right && c.bottom <= work.bottom
            && intersect(c, r).is_none()
    };
    let (bx, by) = candidates.iter().copied().find(|c| fits(c)).unwrap_or((hx, r.bottom - bh - sc(12, dpi)));
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
            status: Status::Ok,
            hot: None,
            ticks: 0,
            footer: Vec::new(),
            footer_rows: 0,
            step: STEP_START,
            since_send: 0,
            moved_since_send: 0,
            sent_once: false,
            stuck: 0,
            paused: false,
        })
    });
    unsafe {
        let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(bar, SW_SHOWNOACTIVATE);
        // Auto-scroll sends the wheel to whatever is under the pointer: put it on the area.
        let _ = SetCursorPos(cx, cy);
        crate::hotkey::set_escape(bar, WM_APP_ESC);
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

/// Scroll the page under the pointer down by `amount` (120 = one wheel notch).
fn wheel(amount: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { mouseData: (-amount) as u32, dwFlags: MOUSEEVENTF_WHEEL, ..Default::default() },
        },
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Grab the area, add whatever scrolled into view, and take the next auto-scroll step.
fn tick() {
    let Some(r) = SESSION.with(|s| s.borrow().as_ref().map(|s| s.rect)) else { return };
    if SESSION.with(|s| s.borrow().as_ref().map_or(true, |s| s.last.is_none())) {
        unsafe {
            let _ = DwmFlush();
        }
    }
    let Some(px) = grab(r) else { return };
    let (mut full, mut at_end) = (false, false);
    let mut send: Option<i32> = None;
    SESSION.with(|cell| {
        let mut b = cell.borrow_mut();
        let Some(s) = b.as_mut() else { return };
        s.ticks += 1;
        let sig = signature(&px, s.w, s.h);
        let before = (s.status, s.rows, s.paused);
        let Some(prev) = s.last.as_ref() else {
            s.stitched = px;
            s.rows = s.h;
            s.last = Some(sig);
            let _ = unsafe { InvalidateRect(Some(s.bar), None, false) };
            return;
        };
        let off = find_offset(prev, &sig, s.last_dy);
        // Rows at the bottom that didn't change: a fixed footer, not page content.
        let still = still_bottom(prev, &sig);
        let went_up = off.is_none() && find_offset(&sig, prev, 0).is_some();
        match off {
            Some(0) => {
                // Same place (maybe with a blinking caret): track the latest look.
                s.last = Some(sig);
                s.status = Status::Ok;
            }
            Some(dy) => {
                if s.footer_rows == 0 && still >= 4 && still + dy < s.h && s.rows >= still {
                    // First time we see a fixed footer: take it off the image for now.
                    s.footer_rows = still;
                    let keep = (s.rows - still) * s.w;
                    s.footer = s.stitched.split_off(keep);
                    s.rows -= still;
                }
                let f = s.footer_rows.min(s.h - dy);
                // Only the rows that just scrolled into view (above any fixed footer).
                s.stitched.extend_from_slice(&px[(s.h - f - dy) * s.w..(s.h - f) * s.w]);
                s.rows += dy;
                s.moved_since_send += dy;
                s.last_dy = dy;
                s.last = Some(sig);
                s.status = Status::Ok;
                full = s.rows >= MAX_HEIGHT || s.rows * s.w >= MAX_PIXELS;
            }
            None if s.ticks <= 3 && s.rows == s.h => {
                // Very start: the first grab may have caught a hover effect or a late
                // repaint. Start over from this frame instead.
                s.stitched = px;
                s.last = Some(sig);
            }
            None => s.status = if went_up { Status::WentUp } else { Status::Lost },
        }

        // Auto-scroll: one step at a time, each once the page has settled.
        s.since_send += 1;
        let (cx, cy) = cursor_pos();
        s.paused = !pt_in(&s.rect, cx, cy);
        let settled = off == Some(0) || s.since_send >= MAX_SETTLE_TICKS;
        if !s.paused && s.status == Status::Ok && settled && s.since_send >= SETTLE_TICKS {
            if s.sent_once {
                if s.moved_since_send == 0 {
                    s.stuck += 1;
                } else {
                    s.stuck = 0;
                    // Aim for ~45% of the area per step: plenty of overlap to line up.
                    let target = (s.h as f32 * 0.45).max(8.0);
                    let ratio = s.moved_since_send as f32 / target;
                    s.step = ((s.step as f32 / ratio.max(0.05)).round() as i32).clamp(STEP_MIN, STEP_MAX);
                }
            }
            if s.stuck >= 2 {
                at_end = true; // two steps that moved nothing: the end of the page
            } else {
                send = Some(s.step);
                s.since_send = 0;
                s.moved_since_send = 0;
                s.sent_once = true;
            }
        } else if s.status == Status::Lost && !s.paused && s.since_send >= MAX_SETTLE_TICKS {
            // Lost track mid auto-scroll: go back up a little and take smaller steps.
            s.step = (s.step / 2).max(STEP_MIN);
            send = Some(-s.step);
            s.since_send = 0;
            s.moved_since_send = 0;
            s.sent_once = false;
        }
        if (s.status, s.rows, s.paused) != before {
            let _ = unsafe { InvalidateRect(Some(s.bar), None, false) };
        }
    });
    if let Some(amount) = send {
        wheel(amount);
    }
    if full || at_end {
        finish();
    }
}

fn close() -> Option<Session> {
    let s = SESSION.with(|s| s.borrow_mut().take())?;
    crate::hotkey::clear_escape();
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
    // Put the fixed footer back, once, at the very bottom.
    let footer = std::mem::take(&mut s.footer);
    s.rows += footer.len() / s.w.max(1);
    s.stitched.extend_from_slice(&footer);
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
                let dot = if s.status != Status::Ok || s.paused { argb(255, 0xFF, 0x9F, 0x0A) } else { argb(255, 0xFF, 0x3B, 0x30) };
                g.fill_ellipse(scf(18.0, dpi), rc.bottom as f32 / 2.0, scf(4.5, dpi), dot);
                let c = if s.hot == Some(0) { argb(255, 0x9E, 0x86, 0xFF) } else { ACCENT };
                g.fill_round_rect(done.left as f32, done.top as f32, rect_w(&done) as f32, rect_h(&done) as f32, scf(8.0, dpi), c);
                let c = if s.hot == Some(1) { argb(255, 62, 62, 70) } else { argb(255, 48, 48, 54) };
                g.fill_round_rect(cancel.left as f32, cancel.top as f32, rect_w(&cancel) as f32, rect_h(&cancel) as f32, scf(8.0, dpi), c);
            }
            let msg = if s.status == Status::WentUp {
                "Scroll down to continue".to_string()
            } else if s.status == Status::Lost {
                "Lost track \u{2014} scroll back up a little".to_string()
            } else if s.paused {
                format!("Paused \u{00B7} {} px \u{00B7} hover the area to resume", group(s.rows))
            } else if s.rows == 0 {
                "Starting\u{2026}".to_string()
            } else {
                format!("Scrolling \u{00B7} {} px \u{00B7} Esc or Done to finish", group(s.rows))
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
        WM_APP_ESC => {
            finish();
            LRESULT(0)
        }
        WM_MOUSELEAVE_ => {
            SESSION.with(|c| {
                if let Some(s) = c.borrow_mut().as_mut() {
                    s.hot = None;
                }
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
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

    #[test]
    fn grouping() {
        assert_eq!(group(7), "7");
        assert_eq!(group(1240), "1,240");
        assert_eq!(group(30000), "30,000");
    }
}
