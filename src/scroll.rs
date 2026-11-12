//! Scrolling capture: pick an area and Marq scrolls it for you, grabbing the area as
//! it goes and stitching the newly revealed rows onto the bottom, building one tall
//! image. It stops by itself at the end of the page. You can also scroll by hand.
//!
//! All the deciding (when to scroll, how far, lining frames up, when it's done) is in
//! `stitch::Scroller`, which is pure and tested against real browser pages. This file
//! is the Windows side: grabbing the area, turning the wheel, and the outline and bar.
//!
//! The outline and the control bar are excluded from screen capture by Windows
//! (WDA_EXCLUDEFROMCAPTURE), so they never end up in the image.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::stitch::{Action, Scroller, Status};
use crate::util::*;

const FRAME_CLASS: &str = "MarqScrollFrame";
const BAR_CLASS: &str = "MarqScrollBar";
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 90;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const WM_MOUSELEAVE_: u32 = 0x02A3;
/// From the keyboard hook: Esc was pressed during the capture.
const WM_APP_ESC: u32 = WM_APP + 40;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    rect: RECT, // screen coordinates of the captured area
    cap: Scroller,
    frame: HWND,
    bar: HWND,
    dpi: u32,
    font: HFONT,
    font_b: HFONT,
    hot: Option<usize>,
    /// What the bar shows, to repaint only when it changes.
    shown: (Status, usize),
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
    let (bw, bh) = (sc(520, dpi), sc(46, dpi));
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
            cap: Scroller::new(w, h),
            frame,
            bar,
            dpi,
            font: ui_font(sc(13, dpi), 400),
            font_b: ui_font(sc(13, dpi), 600),
            hot: None,
            shown: (Status::Starting, 0),
        })
    });
    unsafe {
        let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(bar, SW_SHOWNOACTIVATE);
        // Auto-scroll sends the wheel to whatever is under the pointer: put it on the
        // area, a quarter of the way down, so what it lights up on hover is above the
        // rows each step adds.
        let _ = SetCursorPos(cx, r.top + rect_h(&r) / 4);
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

/// Grab the area and let the Scroller decide what's next.
fn tick() {
    let Some(r) = SESSION.with(|s| s.borrow().as_ref().map(|s| s.rect)) else { return };
    if SESSION.with(|s| s.borrow().as_ref().map_or(true, |s| s.cap.is_empty())) {
        unsafe {
            let _ = DwmFlush(); // the first frame: make sure the snip overlay is gone
        }
    }
    let Some(px) = grab(r) else { return };
    let (cx, cy) = cursor_pos();
    let pointer = pt_in(&r, cx, cy).then(|| ((cx - r.left) as usize, (cy - r.top) as usize));
    let action = SESSION.with(|cell| {
        let mut b = cell.borrow_mut();
        let s = b.as_mut()?;
        let a = s.cap.feed(px, pointer);
        let now = (s.cap.status(), s.cap.rows());
        if now != s.shown {
            s.shown = now;
            let _ = unsafe { InvalidateRect(Some(s.bar), None, false) };
        }
        Some(a)
    });
    // Outside the borrow: the wheel and finishing can re-enter the window procedures.
    match action {
        Some(Action::Wheel(n)) => wheel(n),
        Some(Action::Finish) => finish(),
        _ => {}
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
    let (w, near) = (s.cap.w(), s.rect);
    let (px, rows) = if s.cap.is_empty() {
        // Finished before the first frame: just take what's there now.
        match grab(s.rect) {
            Some(px) => (px, rect_h(&s.rect) as usize),
            None => return,
        }
    } else {
        std::mem::replace(&mut s.cap, Scroller::new(0, 0)).into_image()
    };
    if rows == 0 {
        return;
    }
    let img = Image { w: w as i32, h: rows as i32, px };
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
                let dot = match s.cap.status() {
                    Status::Scrolling | Status::Starting => argb(255, 0xFF, 0x3B, 0x30),
                    _ => argb(255, 0xFF, 0x9F, 0x0A),
                };
                g.fill_ellipse(scf(18.0, dpi), rc.bottom as f32 / 2.0, scf(4.5, dpi), dot);
                let c = if s.hot == Some(0) { argb(255, 0x9E, 0x86, 0xFF) } else { ACCENT };
                g.fill_round_rect(done.left as f32, done.top as f32, rect_w(&done) as f32, rect_h(&done) as f32, scf(8.0, dpi), c);
                let c = if s.hot == Some(1) { argb(255, 62, 62, 70) } else { argb(255, 48, 48, 54) };
                g.fill_round_rect(cancel.left as f32, cancel.top as f32, rect_w(&cancel) as f32, rect_h(&cancel) as f32, scf(8.0, dpi), c);
            }
            let rows = group(s.cap.rows());
            let msg = match s.cap.status() {
                Status::Starting => "Starting\u{2026}".to_string(),
                Status::Scrolling => format!("Scrolling \u{00B7} {rows} px \u{00B7} Esc or Done to finish"),
                Status::Paused => format!("Paused \u{00B7} {rows} px \u{00B7} hover the area to resume"),
                Status::WentUp => format!("Scroll down to continue \u{00B7} {rows} px"),
                Status::Lost => "Lost track \u{00B7} scroll up a little, or click Done".to_string(),
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
