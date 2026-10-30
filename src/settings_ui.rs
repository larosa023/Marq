//! Small Settings window: snip shortcut and Start with Windows.

use std::cell::Cell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::hotkey;
use crate::settings;
use crate::util::*;

const CLASS: &str = "MarqSettings";
const WM_APP_RECORDED: u32 = WM_APP + 20;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const BG: u32 = argb(255, 28, 28, 32);
const FIELD: u32 = argb(255, 40, 40, 46);
const TEXT: u32 = argb(255, 238, 238, 244);
const MUTED: u32 = argb(255, 150, 150, 162);
const ERROR: u32 = argb(255, 0xFF, 0x6B, 0x5E);
const OK: u32 = argb(255, 0x34, 0xC7, 0x59);

// Layout in 96-DPI units.
const CW: i32 = 440;
const CH: i32 = 236;

thread_local! {
    static CURRENT: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Change,
    Reset,
    Autostart,
    Done,
}

struct Settings {
    back: Option<Dib>,
    dpi: u32,
    font: HFONT,
    font_b: HFONT,
    font_s: HFONT,
    mods: u32,
    vk: u32,
    recording: bool,
    autostart: bool,
    note: Option<(String, u32)>,
    hot: Option<Btn>,
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.font_b.into());
            let _ = DeleteObject(self.font_s.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_ARROW, WNDCLASS_STYLES(0));
}

pub fn open() {
    let existing = CURRENT.with(|c| c.get());
    if !existing.0.is_null() {
        force_foreground(existing);
        return;
    }
    let (cx, cy) = cursor_pos();
    let dpi = dpi_for_point(cx, cy);
    let work = work_area_for_point(cx, cy);
    let (mods, vk) = settings::load_hotkey();
    let st = Box::new(Settings {
        back: None,
        dpi,
        font: ui_font(sc(13, dpi), 400),
        font_b: ui_font(sc(13, dpi), 600),
        font_s: ui_font(sc(12, dpi), 400),
        mods,
        vk,
        recording: false,
        autostart: settings::autostart_enabled(),
        note: None,
        hot: None,
    });
    unsafe {
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut wr = rect(0, 0, sc(CW, dpi), sc(CH, dpi));
        let _ = AdjustWindowRectExForDpi(&mut wr, style, false, WINDOW_EX_STYLE(0), dpi);
        let (ww, wh) = (rect_w(&wr), rect_h(&wr));
        let x = work.left + (rect_w(&work) - ww) / 2;
        let y = work.top + (rect_h(&work) - wh) / 2;
        let cls = WStr::new(CLASS);
        let title = WStr::new("Marq Settings");
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0), cls.p(), title.p(), style, x, y, ww, wh,
            None, None, Some(hinstance()), None,
        ) else {
            return;
        };
        set_state(hwnd, st);
        modern_chrome(hwnd, true, false);
        CURRENT.with(|c| c.set(hwnd));
        let _ = ShowWindow(hwnd, SW_SHOW);
        force_foreground(hwnd);
    }
}

fn buttons(dpi: u32) -> [(Btn, RECT); 4] {
    let s = |v| sc(v, dpi);
    [
        (Btn::Change, rect(s(272), s(48), s(344), s(84))),
        (Btn::Reset, rect(s(352), s(48), s(416), s(84))),
        (Btn::Autostart, rect(s(24), s(140), s(260), s(166))),
        (Btn::Done, rect(s(336), s(188), s(416), s(220))),
    ]
}

fn field_rect(dpi: u32) -> RECT {
    rect(sc(24, dpi), sc(48, dpi), sc(262, dpi), sc(84, dpi))
}

fn hit(st: &Settings, x: i32, y: i32) -> Option<Btn> {
    buttons(st.dpi).into_iter().find(|(_, r)| pt_in(r, x, y)).map(|(b, _)| b)
}

fn redraw(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn set_recording(hwnd: HWND, st: &mut Settings, on: bool) {
    st.recording = on;
    if on {
        st.note = Some(("Press the keys you want. Esc cancels.".into(), MUTED));
        hotkey::start_recording(hwnd, WM_APP_RECORDED);
    } else {
        hotkey::stop_recording();
    }
    redraw(hwnd);
}

fn apply(hwnd: HWND, st: &mut Settings, mods: u32, vk: u32) {
    st.mods = mods;
    st.vk = vk;
    settings::save_hotkey(mods, vk);
    crate::apply_hotkey(mods, vk);
    st.note = Some((format!("Saved. Press {} to snip.", hotkey::label(mods, vk)), OK));
    redraw(hwnd);
}

fn paint(hwnd: HWND, st: &mut Settings) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let need = match &st.back {
            Some(b) => b.w != rc.right || b.h != rc.bottom,
            None => true,
        };
        if need {
            st.back = None;
            st.back = Dib::new(rc.right.max(1), rc.bottom.max(1));
        }
        let Some(back) = st.back.as_ref() else {
            let _ = EndPaint(hwnd, &ps);
            return;
        };
        let dpi = st.dpi;
        let s = |v| sc(v, dpi);
        let f = |v| scf(v, dpi);
        let field = field_rect(dpi);
        let btns = buttons(dpi);

        if let Some(g) = Graphics::from_hdc(back.dc) {
            g.fill_round_rect(0.0, 0.0, rc.right as f32, rc.bottom as f32, 0.0, BG);
            // Shortcut field.
            let fr = field;
            g.fill_round_rect(fr.left as f32, fr.top as f32, rect_w(&fr) as f32, rect_h(&fr) as f32, f(8.0), FIELD);
            if st.recording {
                g.stroke_round_rect(
                    fr.left as f32 + 0.75, fr.top as f32 + 0.75,
                    rect_w(&fr) as f32 - 1.5, rect_h(&fr) as f32 - 1.5,
                    f(8.0), f(1.5), ACCENT,
                );
            }
            // Buttons.
            for (b, r) in btns.iter() {
                let (x, y, w, h) = (r.left as f32, r.top as f32, rect_w(r) as f32, rect_h(r) as f32);
                let hot = st.hot == Some(*b);
                match b {
                    Btn::Autostart => {
                        // Toggle switch.
                        let tw = f(40.0);
                        let th = f(22.0);
                        let ty = y + (h - th) / 2.0;
                        let on = st.autostart;
                        let track = if on { ACCENT } else if hot { argb(255, 80, 80, 90) } else { argb(255, 64, 64, 72) };
                        g.fill_round_rect(x, ty, tw, th, th / 2.0, track);
                        let knob_x = if on { x + tw - th / 2.0 } else { x + th / 2.0 };
                        g.fill_ellipse(knob_x, ty + th / 2.0, th / 2.0 - f(3.0), argb(255, 255, 255, 255));
                    }
                    Btn::Done => {
                        let c = if hot { argb(255, 0x9E, 0x86, 0xFF) } else { ACCENT };
                        g.fill_round_rect(x, y, w, h, f(8.0), c);
                    }
                    _ => {
                        let c = if hot { argb(255, 62, 62, 70) } else { argb(255, 48, 48, 54) };
                        g.fill_round_rect(x, y, w, h, f(8.0), c);
                    }
                }
            }
            // Divider.
            g.fill_round_rect(f(24.0), f(122.0), f((CW - 48) as f32), 1.0, 0.0, argb(255, 50, 50, 58));
        }

        let single = DT_SINGLELINE | DT_VCENTER;
        draw_text(back.dc, "Snip shortcut", rect(s(24), s(18), s(400), s(42)), st.font_b, TEXT, DT_LEFT | single);
        let shown = if st.recording { "Press a shortcut\u{2026}".to_string() } else { hotkey::label(st.mods, st.vk) };
        draw_text(back.dc, &shown, field, st.font_b, if st.recording { MUTED } else { TEXT }, DT_CENTER | single);
        if let Some((msg, color)) = &st.note {
            draw_text(back.dc, msg, rect(s(24), s(90), s(416), s(112)), st.font_s, *color, DT_LEFT | single | DT_END_ELLIPSIS);
        }
        for (b, r) in btns.iter() {
            match b {
                Btn::Change => {
                    let t = if st.recording { "Cancel" } else { "Change" };
                    draw_text(back.dc, t, *r, st.font, TEXT, DT_CENTER | single);
                }
                Btn::Reset => draw_text(back.dc, "Reset", *r, st.font, TEXT, DT_CENTER | single),
                Btn::Done => draw_text(back.dc, "Done", *r, st.font_b, argb(255, 255, 255, 255), DT_CENTER | single),
                Btn::Autostart => {
                    let lr = rect(r.left + s(52), r.top, r.right + s(120), r.bottom);
                    draw_text(back.dc, "Start with Windows", lr, st.font, TEXT, DT_LEFT | single);
                }
            }
        }
        let ver = format!("Marq v{}", crate::updater::CURRENT);
        draw_text(back.dc, &ver, rect(s(24), s(188), s(300), s(220)), st.font_s, MUTED, DT_LEFT | single);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back.dc), 0, 0, SRCCOPY);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = get_state::<Settings>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            if loword(lp.0 as usize) as u32 == HTCLIENT {
                let (x, y) = cursor_pos();
                let mut p = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut p);
                let c = if hit(st, p.x, p.y).is_some() { IDC_HAND } else { IDC_ARROW };
                SetCursor(LoadCursorW(None, c).ok());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let h = hit(st, x, y);
            if h != st.hot {
                st.hot = h;
                redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lp);
            match hit(st, x, y) {
                Some(Btn::Change) => {
                    let on = !st.recording;
                    if !on {
                        st.note = None;
                    }
                    set_recording(hwnd, st, on);
                }
                Some(Btn::Reset) => {
                    set_recording(hwnd, st, false);
                    let (m, k) = hotkey::DEFAULT;
                    apply(hwnd, st, m, k);
                }
                Some(Btn::Autostart) => {
                    st.autostart = !st.autostart;
                    settings::set_autostart(st.autostart);
                    st.autostart = settings::autostart_enabled();
                    redraw(hwnd);
                }
                Some(Btn::Done) => {
                    let _ = DestroyWindow(hwnd);
                }
                None => {}
            }
            LRESULT(0)
        }
        WM_APP_RECORDED => {
            if !st.recording {
                return LRESULT(0);
            }
            set_recording(hwnd, st, false);
            if wp.0 == hotkey::RECORD_CANCELLED {
                st.note = None;
            } else {
                let (mods, vk) = (wp.0 as u32, lp.0 as u32);
                match hotkey::problem(mods, vk) {
                    Some(why) => st.note = Some((format!("{} isn't available. {}", hotkey::label(mods, vk), why), ERROR)),
                    None => apply(hwnd, st, mods, vk),
                }
            }
            redraw(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE | VK_RETURN if !st.recording => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // Clicking away stops listening, so the next keys you type elsewhere are yours.
            if loword(wp.0) == 0 && st.recording {
                st.note = None;
                set_recording(hwnd, st, false);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DESTROY => {
            hotkey::stop_recording();
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(take_state::<Settings>(hwnd));
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
