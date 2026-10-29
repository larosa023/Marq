//! Grabbing the screen and finding window / monitor rectangles.

use windows::core::BOOL;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util::Dib;

/// The whole virtual desktop (all monitors), plus its top-left in screen coordinates.
pub struct Frozen {
    pub dib: Dib,
    pub origin: (i32, i32),
}

pub fn capture_screen() -> Option<Frozen> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let dib = Dib::new(w, h)?;
        let screen = GetDC(None);
        let _ = BitBlt(dib.dc, 0, 0, w, h, Some(screen), x, y, SRCCOPY); // no CAPTUREBLT: it makes the cursor flicker, and DWM already composes layered windows
        ReleaseDC(None, screen);
        let _ = GdiFlush();
        for p in dib.pixels_mut().iter_mut() {
            *p |= 0xFF00_0000;
        }
        Some(Frozen { dib, origin: (x, y) })
    }
}

/// Visible top-level windows in z-order (topmost first), as screen rects without drop shadows.
pub fn window_rects() -> Vec<RECT> {
    let mut out: Vec<RECT> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<RECT>);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return BOOL(1);
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return BOOL(1);
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TRANSPARENT.0 != 0 {
        return BOOL(1);
    }
    // Invisible layered windows (alpha 0) some apps keep around: not something to snip.
    if ex & WS_EX_LAYERED.0 != 0 {
        let mut alpha = 255u8;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        if GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)).is_ok()
            && (flags.0 & LWA_ALPHA.0) != 0
            && alpha == 0
        {
            return BOOL(1);
        }
    }
    // Skip the desktop itself; hovering empty desktop falls back to the monitor.
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls);
    let cls = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
    if cls == "Progman" || cls == "WorkerW" {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut _ as *mut _,
        std::mem::size_of::<RECT>() as u32,
    )
    .is_err()
    {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    if r.right - r.left >= 24 && r.bottom - r.top >= 24 {
        out.push(r);
    }
    BOOL(1)
}

pub fn monitor_rects() -> Vec<RECT> {
    let mut out: Vec<RECT> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(mon_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

unsafe extern "system" fn mon_proc(_m: HMONITOR, _dc: HDC, r: *mut RECT, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<RECT>);
    out.push(*r);
    BOOL(1)
}
