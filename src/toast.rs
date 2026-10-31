//! Small preview card shown after a snip ("Copied to clipboard — click to mark up"),
//! or after copying text ("Text copied" plus the first few lines).

use std::cell::Cell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::util::*;

const CLASS: &str = "MarqToast";
const TIMER_CLOSE: usize = 1;
const SHOW_MS: u32 = 5000;
const WM_MOUSELEAVE: u32 = 0x02A3;

thread_local! {
    static CURRENT: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

struct Toast {
    copied: bool,
    saved: Option<std::path::PathBuf>, // where the snip was saved, if it was
    img: Option<Image>,  // handed to the editor on click
    thumb: Option<Dib>,  // pre-scaled once, so painting is a plain blit (None for text cards)
    text: Vec<String>,   // text card: preview lines
    thumb_at: (i32, i32),
    back: Option<Dib>,
    dpi: u32,
    font_b: HFONT,
    font: HFONT,
    hot: bool,
}

impl Drop for Toast {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font_b.into());
            let _ = DeleteObject(self.font.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_HAND, CS_DROPSHADOW);
}

/// Close the card if one is showing. Returns true if there was one.
pub fn close_current() -> bool {
    let h = CURRENT.with(|c| c.get());
    if !h.0.is_null() {
        unsafe {
            let _ = DestroyWindow(h);
        }
        return true;
    }
    false
}

/// Show the card near the bottom-right of the monitor where the snip was taken.
pub fn show(img: Image, near: RECT, copied: bool, saved: Option<std::path::PathBuf>) {
    close_current();
    let cx = (near.left + near.right) / 2;
    let cy = (near.top + near.bottom) / 2;
    let work = work_area_for_point(cx, cy);
    let dpi = dpi_for_point(cx, cy);

    let pad = sc(10, dpi);
    let max_w = sc(300, dpi) as f32;
    let max_h = sc(170, dpi) as f32;
    let s = (max_w / img.w as f32).min(max_h / img.h as f32).min(1.0);
    let tw = ((img.w as f32 * s).round() as i32).max(1);
    let th = ((img.h as f32 * s).round() as i32).max(1);
    let text_h = sc(44, dpi);
    let w = (tw + pad * 2).max(sc(240, dpi));
    let h = th + pad * 2 + text_h;
    let x = work.right - w - sc(16, dpi);
    let y = work.bottom - h - sc(16, dpi);

    // Scale the thumbnail once.
    let Some(thumb) = Dib::new(tw, th) else { return };
    {
        let Some(src) = Dib::from_image(&img) else { return };
        unsafe {
            SetStretchBltMode(thumb.dc, HALFTONE);
            let _ = SetBrushOrgEx(thumb.dc, 0, 0, None);
            let _ = StretchBlt(thumb.dc, 0, 0, tw, th, Some(src.dc), 0, 0, src.w, src.h, SRCCOPY);
            let _ = GdiFlush();
        }
    } // full-size temporary freed here

    let st = Box::new(Toast {
        copied,
        saved,
        img: Some(img),
        thumb: Some(thumb),
        text: Vec::new(),
        thumb_at: ((w - tw) / 2, pad),
        back: Dib::new(w, h),
        dpi,
        font_b: ui_font(sc(13, dpi), 600),
        font: ui_font(sc(12, dpi), 400),
        hot: false,
    });

    create(st, x, y, w, h);
}

/// "Text copied" card with a short preview, or the reason nothing was copied.
pub fn show_text(result: &Result<String, String>, near: RECT) {
    close_current();
    let cx = (near.left + near.right) / 2;
    let cy = (near.top + near.bottom) / 2;
    let work = work_area_for_point(cx, cy);
    let dpi = dpi_for_point(cx, cy);
    let (copied, mut lines) = match result {
        Ok(t) => (true, t.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect::<Vec<_>>()),
        Err(e) => (false, vec![e.clone()]),
    };
    let total = lines.len();
    lines.truncate(3);
    if copied && total > 3 {
        lines.push(format!("+{} more line{}", total - 3, if total - 3 == 1 { "" } else { "s" }));
    }
    let w = sc(300, dpi);
    let h = sc(14 + 22 + 6, dpi) + sc(18, dpi) * lines.len() as i32 + sc(12, dpi);
    let x = work.right - w - sc(16, dpi);
    let y = work.bottom - h - sc(16, dpi);
    let st = Box::new(Toast {
        copied,
        saved: None,
        img: None,
        thumb: None,
        text: lines,
        thumb_at: (0, 0),
        back: Dib::new(w, h),
        dpi,
        font_b: ui_font(sc(13, dpi), 600),
        font: ui_font(sc(12, dpi), 400),
        hot: false,
    });
    create(st, x, y, w, h);
}

fn create(st: Box<Toast>, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let cls = WStr::new(CLASS);
        let title = WStr::new("Marq");
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            cls.p(),
            title.p(),
            WS_POPUP,
            x,
            y,
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
        modern_chrome(hwnd, true, true);
        CURRENT.with(|c| c.set(hwnd));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), TIMER_CLOSE, SHOW_MS, None);
    }
}

fn paint(hwnd: HWND, st: &mut Toast) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        if let (Some(back), None) = (st.back.as_ref(), st.thumb.as_ref()) {
            // Text card.
            let dpi = st.dpi;
            let bg = if st.hot { argb(255, 44, 44, 50) } else { argb(255, 32, 32, 36) };
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.fill_round_rect(0.0, 0.0, rc.right as f32, rc.bottom as f32, 0.0, bg);
                let dot = if st.copied { argb(255, 0x34, 0xC7, 0x59) } else { argb(255, 0xFF, 0x9F, 0x0A) };
                g.fill_ellipse(scf(18.0, dpi), scf(25.0, dpi), scf(4.0, dpi), dot);
            }
            let tx = sc(30, dpi);
            draw_text(
                back.dc, if st.copied { "Text copied" } else { "Couldn't copy text" },
                rect(tx, sc(14, dpi), rc.right - sc(12, dpi), sc(36, dpi)),
                st.font_b, argb(255, 245, 245, 250), DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            for (i, line) in st.text.iter().enumerate() {
                let top = sc(42, dpi) + sc(18, dpi) * i as i32;
                draw_text(
                    back.dc, line,
                    rect(tx, top, rc.right - sc(12, dpi), top + sc(18, dpi)),
                    st.font, argb(255, 170, 170, 182),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back.dc), 0, 0, SRCCOPY);
        } else if let (Some(back), Some(thumb)) = (st.back.as_ref(), st.thumb.as_ref()) {
            let bg = if st.hot { argb(255, 44, 44, 50) } else { argb(255, 32, 32, 36) };
            let (tx0, ty0) = st.thumb_at;
            let (tw, th) = (thumb.w, thumb.h);
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.fill_round_rect(0.0, 0.0, rc.right as f32, rc.bottom as f32, 0.0, bg);
            }
            let _ = BitBlt(back.dc, tx0, ty0, tw, th, Some(thumb.dc), 0, 0, SRCCOPY);
            let dpi = st.dpi;
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.stroke_round_rect(
                    tx0 as f32 - 0.5, ty0 as f32 - 0.5, tw as f32 + 1.0, th as f32 + 1.0,
                    0.0, 1.0, argb(50, 255, 255, 255),
                );
                // Green "done" dot beside the caption.
                let cy = (ty0 + th) as f32 + scf(22.0, dpi);
                let dot = if st.copied { argb(255, 0x34, 0xC7, 0x59) } else { argb(255, 0xFF, 0x9F, 0x0A) };
                g.fill_ellipse(scf(18.0, dpi), cy, scf(4.0, dpi), dot);
            }
            let tx = sc(30, dpi);
            let row = ty0 + th + sc(12, dpi);
            draw_text(
                back.dc, if st.copied { "Copied to clipboard" } else { "Couldn't reach the clipboard" },
                rect(tx, row, rc.right - sc(10, dpi), row + sc(20, dpi)),
                st.font_b, argb(255, 245, 245, 250), DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            draw_text(
                back.dc,
                match (st.copied, st.saved.is_some()) {
                    (true, true) => "Saved to Screenshots \u{00B7} click to mark up",
                    (true, false) => "Click to mark up \u{00B7} right-click to dismiss",
                    (false, _) => "Click to open it and copy again",
                },
                rect(tx, row + sc(18, dpi), rc.right - sc(10, dpi), row + sc(36, dpi)),
                st.font, argb(255, 160, 160, 172), DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back.dc), 0, 0, SRCCOPY);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

fn set_hot(hwnd: HWND, st: &mut Toast, hot: bool) {
    if st.hot != hot {
        st.hot = hot;
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = get_state::<Toast>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            if !st.hot {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            set_hot(hwnd, st, true);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            set_hot(hwnd, st, false);
            // Restart the countdown so it doesn't vanish the instant the pointer leaves.
            SetTimer(Some(hwnd), TIMER_CLOSE, 2000, None);
            LRESULT(0)
        }
        WM_TIMER => {
            // Stay open while the pointer is over the card.
            if !st.hot {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let img = st.img.take();
            let saved = st.saved.take();
            let _ = DestroyWindow(hwnd);
            if let Some(img) = img {
                crate::editor::open(img, saved);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = KillTimer(Some(hwnd), TIMER_CLOSE);
            drop(take_state::<Toast>(hwnd));
            CURRENT.with(|c| {
                if c.get() == hwnd {
                    c.set(HWND(std::ptr::null_mut()));
                }
            });
            trim_memory();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
