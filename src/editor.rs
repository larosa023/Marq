//! Minimal markup editor: pen, highlighter, arrow, five colors, undo.
//! Every finished stroke re-copies the image to the clipboard automatically.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{argb, Graphics};
use crate::util::*;

const CLASS: &str = "MarqEditor";
const TIMER_STATUS: usize = 1;
const ACCENT: u32 = argb(255, 0x8B, 0x6F, 0xFF);
const BG: u32 = argb(255, 24, 24, 27);
const BAR: u32 = argb(255, 32, 32, 36);

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

struct Editor {
    base: Dib,
    committed: Dib,
    work: Dib,
    strokes: Vec<Stroke>,
    current: Option<Stroke>,
    tool: Tool,
    color: usize,
    dpi: u32,
    font: HFONT,
    view: RECT,
    scale: f32,
    buttons: Vec<(Btn, RECT)>,
    hot: Option<usize>,
    status: Option<&'static str>,
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
        }
    }
}

pub fn register() {
    register_class(&WStr::new(CLASS), Some(wndproc), IDC_ARROW, CS_HREDRAW | CS_VREDRAW);
}

pub fn open(img: Image) {
    let Some(base) = Dib::from_image(&img) else { return };
    let Some(committed) = Dib::from_image(&img) else { return };
    let Some(work) = Dib::from_image(&img) else { return };

    let (cx, cy) = cursor_pos();
    let dpi = dpi_for_point(cx, cy);
    let work_area = work_area_for_point(cx, cy);

    // Size the window so the image fits in ~85% of the screen, never upscaled.
    let bar = sc(52, dpi);
    let margin = sc(12, dpi);
    let max_w = ((work_area.right - work_area.left) as f32 * 0.85) as i32;
    let max_h = ((work_area.bottom - work_area.top) as f32 * 0.85) as i32;
    let s = ((max_w - margin * 2) as f32 / img.w as f32)
        .min((max_h - bar - margin * 2) as f32 / img.h as f32)
        .min(1.0);
    let cw = ((img.w as f32 * s) as i32 + margin * 2).max(sc(560, dpi));
    let ch = (img.h as f32 * s) as i32 + bar + margin * 2;

    let st = Box::new(Editor {
        base,
        committed,
        work,
        strokes: Vec::new(),
        current: None,
        tool: Tool::Pen,
        color: 0,
        dpi,
        font: ui_font(sc(13, dpi), 500),
        view: RECT::default(),
        scale: 1.0,
        buttons: Vec::new(),
        hot: None,
        status: None,
    });

    unsafe {
        let style = WS_OVERLAPPEDWINDOW;
        let mut wr = rect(0, 0, cw, ch);
        let _ = AdjustWindowRectExForDpi(&mut wr, style, false, WINDOW_EX_STYLE(0), dpi);
        let ww = wr.right - wr.left;
        let wh = wr.bottom - wr.top;
        let x = work_area.left + (work_area.right - work_area.left - ww) / 2;
        let y = work_area.top + (work_area.bottom - work_area.top - wh) / 2;

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
        let _ = SetForegroundWindow(hwnd);
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

fn layout(hwnd: HWND, st: &mut Editor) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let dpi = st.dpi;
    let bar = sc(52, dpi);
    let margin = sc(12, dpi);

    // Fit image under the toolbar.
    let aw = (rc.right - margin * 2).max(1) as f32;
    let ah = (rc.bottom - bar - margin * 2).max(1) as f32;
    let s = (aw / st.base.w as f32).min(ah / st.base.h as f32).min(1.0);
    let w = (st.base.w as f32 * s).round() as i32;
    let h = (st.base.h as f32 * s).round() as i32;
    let left = (rc.right - w) / 2;
    let top = bar + margin + ((ah as i32 - h) / 2).max(0);
    st.view = rect(left, top, left + w, top + h);
    st.scale = s;

    // Toolbar buttons.
    st.buttons.clear();
    let bh = sc(32, dpi);
    let y = (bar - bh) / 2;
    let mut x = sc(10, dpi);
    let gap = sc(4, dpi);
    let pad = sc(14, dpi);
    let tw = |t: &str| unsafe {
        let dc = GetDC(None);
        let w = text_width(dc, t, st.font);
        ReleaseDC(None, dc);
        w
    };
    for (t, label) in [(Tool::Pen, "Pen"), (Tool::Highlighter, "Highlighter"), (Tool::Arrow, "Arrow")] {
        let w = tw(label) + pad * 2;
        st.buttons.push((Btn::Tool(t), rect(x, y, x + w, y + bh)));
        x += w + gap;
    }
    x += sc(12, dpi);
    for i in 0..COLORS.len() {
        st.buttons.push((Btn::Color(i), rect(x, y, x + bh, y + bh)));
        x += bh + gap;
    }
    x += sc(12, dpi);
    for (b, label) in [(Btn::Undo, "Undo"), (Btn::Copy, "Copy")] {
        let w = tw(label) + pad * 2;
        st.buttons.push((b, rect(x, y, x + w, y + bh)));
        x += w + gap;
    }
}

fn label(b: Btn) -> &'static str {
    match b {
        Btn::Tool(Tool::Pen) => "Pen",
        Btn::Tool(Tool::Highlighter) => "Highlighter",
        Btn::Tool(Tool::Arrow) => "Arrow",
        Btn::Undo => "Undo",
        Btn::Copy => "Copy",
        Btn::Color(_) => "",
    }
}

fn rebuild_committed(st: &mut Editor) {
    st.committed.copy_from(&st.base);
    if let Some(g) = Graphics::from_hdc(st.committed.dc) {
        for s in &st.strokes {
            draw_stroke(&g, s);
        }
    }
    st.work.copy_from(&st.committed);
}

fn copy_now(hwnd: HWND, st: &mut Editor) {
    let img = st.committed.to_image();
    st.status = Some(if copy_to_clipboard(hwnd, &img) { "Copied \u{2713}" } else { "Couldn't copy" });
    unsafe {
        SetTimer(Some(hwnd), TIMER_STATUS, 1600, None);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn to_image_space(st: &Editor, x: i32, y: i32) -> (f32, f32) {
    (
        (x - st.view.left) as f32 / st.scale,
        (y - st.view.top) as f32 / st.scale,
    )
}

fn undo(hwnd: HWND, st: &mut Editor) {
    if st.strokes.pop().is_some() {
        rebuild_committed(st);
        copy_now(hwnd, st);
    }
}

fn press(hwnd: HWND, st: &mut Editor, b: Btn) {
    match b {
        Btn::Tool(t) => st.tool = t,
        Btn::Color(i) => st.color = i,
        Btn::Undo => undo(hwnd, st),
        Btn::Copy => copy_now(hwnd, st),
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn paint(hwnd: HWND, st: &mut Editor) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let dpi = st.dpi;
        let bar = sc(52, dpi);
        if let Some(back) = Dib::new(rc.right, rc.bottom) {
            if let Some(g) = Graphics::from_hdc(back.dc) {
                g.fill_round_rect(0.0, 0.0, rc.right as f32, rc.bottom as f32, 0.0, BG);
                g.fill_round_rect(0.0, 0.0, rc.right as f32, bar as f32, 0.0, BAR);
                for (i, (b, r)) in st.buttons.iter().enumerate() {
                    let (x, y, w, h) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
                    match b {
                        Btn::Color(ci) => {
                            let c = COLORS[*ci];
                            let cxp = x + w / 2.0;
                            let cyp = y + h / 2.0;
                            if *ci == st.color {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(2.0, dpi), argb(255, 255, 255, 255));
                            } else if st.hot == Some(i) {
                                g.stroke_ellipse(cxp, cyp, w / 2.0 - 1.5, scf(1.5, dpi), argb(90, 255, 255, 255));
                            }
                            g.fill_ellipse(cxp, cyp, w / 2.0 - scf(6.0, dpi), c);
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
                let c = if active { argb(255, 255, 255, 255) } else { argb(255, 205, 205, 215) };
                draw_text(back.dc, label(*b), *r, st.font, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            if let Some(s) = st.status {
                let r = rect(rc.right - sc(160, dpi), 0, rc.right - sc(14, dpi), bar);
                draw_text(back.dc, s, r, st.font, argb(255, 0x34, 0xC7, 0x59), DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
            }

            // Image.
            let v = st.view;
            if st.scale >= 0.999 {
                let _ = BitBlt(back.dc, v.left, v.top, st.work.w, st.work.h, Some(st.work.dc), 0, 0, SRCCOPY);
            } else {
                SetStretchBltMode(back.dc, HALFTONE);
                let _ = SetBrushOrgEx(back.dc, 0, 0, None);
                let _ = StretchBlt(
                    back.dc, v.left, v.top, v.right - v.left, v.bottom - v.top,
                    Some(st.work.dc), 0, 0, st.work.w, st.work.h, SRCCOPY,
                );
            }
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back.dc), 0, 0, SRCCOPY);
        }
        let _ = EndPaint(hwnd, &ps);
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
            layout(hwnd, st);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: sc(580, st.dpi), y: sc(260, st.dpi) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            st.dpi = hiword(wp.0) as u32;
            let _ = DeleteObject(st.font.into());
            st.font = ui_font(sc(13, st.dpi), 500);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            layout(hwnd, st);
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
            if pt_in(&st.view, x, y) {
                let p = to_image_space(st, x, y);
                st.current = Some(Stroke {
                    tool: st.tool,
                    color: COLORS[st.color],
                    width: stroke_width(st.tool, st.dpi),
                    pts: vec![p, p],
                });
                SetCapture(hwnd);
                st.work.copy_from(&st.committed);
                if let (Some(g), Some(s)) = (Graphics::from_hdc(st.work.dc), st.current.as_ref()) {
                    draw_stroke(&g, s);
                }
                let _ = InvalidateRect(Some(hwnd), Some(&st.view), false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            let hot = st.buttons.iter().position(|(_, r)| pt_in(r, x, y));
            if hot != st.hot {
                st.hot = hot;
                let mut bar = RECT::default();
                let _ = GetClientRect(hwnd, &mut bar);
                bar.bottom = sc(52, st.dpi);
                let _ = InvalidateRect(Some(hwnd), Some(&bar), false);
            }
            if st.current.is_some() {
                let p = to_image_space(st, x, y);
                if let Some(s) = st.current.as_mut() {
                    if s.tool == Tool::Arrow {
                        let last = s.pts.len() - 1;
                        s.pts[last] = p;
                    } else {
                        s.pts.push(p);
                    }
                }
                st.work.copy_from(&st.committed);
                if let (Some(g), Some(s)) = (Graphics::from_hdc(st.work.dc), st.current.as_ref()) {
                    draw_stroke(&g, s);
                }
                let _ = InvalidateRect(Some(hwnd), Some(&st.view), false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(s) = st.current.take() {
                let _ = ReleaseCapture();
                st.strokes.push(s);
                if let (Some(g), Some(s)) = (Graphics::from_hdc(st.committed.dc), st.strokes.last()) {
                    draw_stroke(&g, s);
                }
                st.work.copy_from(&st.committed);
                copy_now(hwnd, st);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => {
                    let _ = DestroyWindow(hwnd);
                }
                VK_Z if ctrl => undo(hwnd, st),
                VK_C if ctrl => copy_now(hwnd, st),
                VK_P => press(hwnd, st, Btn::Tool(Tool::Pen)),
                VK_H => press(hwnd, st, Btn::Tool(Tool::Highlighter)),
                VK_A => press(hwnd, st, Btn::Tool(Tool::Arrow)),
                k if (VK_1.0..=VK_5.0).contains(&k.0) => {
                    press(hwnd, st, Btn::Color((k.0 - VK_1.0) as usize))
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_STATUS {
                let _ = KillTimer(Some(hwnd), TIMER_STATUS);
                st.status = None;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(take_state::<Editor>(hwnd));
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
