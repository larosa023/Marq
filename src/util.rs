//! Shared helpers: strings, images, DIBs, clipboard, DPI, fonts and text.

use windows::core::{PCWSTR, BOOL};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Null-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Keeps a UTF-16 buffer alive while handing out a PCWSTR.
pub struct WStr(pub Vec<u16>);
impl WStr {
    pub fn new(s: &str) -> WStr {
        WStr(wide(s))
    }
    pub fn p(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

/// Copy a &str into a fixed-size UTF-16 array (truncating), e.g. for NOTIFYICONDATAW.
pub fn fill_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
    let mut i = 0;
    for c in s.encode_utf16() {
        if i >= N - 1 {
            break;
        }
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

pub fn loword(v: usize) -> i32 {
    (v & 0xFFFF) as u16 as i16 as i32
}
pub fn hiword(v: usize) -> i32 {
    ((v >> 16) & 0xFFFF) as u16 as i16 as i32
}
pub fn lparam_point(lp: LPARAM) -> (i32, i32) {
    (loword(lp.0 as usize), hiword(lp.0 as usize))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// 32-bit BGRA image, top-down rows. Alpha is always 0xFF.
#[derive(Clone)]
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

/// Copy a rectangle out of a top-down pixel buffer.
pub fn crop(px: &[u32], pw: i32, ph: i32, r: RECT) -> Image {
    let x0 = r.left.clamp(0, pw);
    let y0 = r.top.clamp(0, ph);
    let x1 = r.right.clamp(0, pw);
    let y1 = r.bottom.clamp(0, ph);
    let w = (x1 - x0).max(0);
    let h = (y1 - y0).max(0);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in y0..y1 {
        let row = (y * pw) as usize;
        out.extend_from_slice(&px[row + x0 as usize..row + x1 as usize]);
    }
    Image { w, h, px: out }
}

/// A GDI DIB section that owns its pixels; used for fast blitting and GDI+ drawing.
pub struct Dib {
    pub dc: HDC,
    pub bmp: HBITMAP,
    old: HGDIOBJ,
    pub bits: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32) -> Option<Dib> {
        unsafe {
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w.max(1),
                    biHeight: -h.max(1), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Dib { dc, bmp, old, bits: bits as *mut u32, w: w.max(1), h: h.max(1) })
        }
    }

    pub fn from_image(img: &Image) -> Option<Dib> {
        let d = Dib::new(img.w, img.h)?;
        d.pixels_mut().copy_from_slice(&img.px);
        Some(d)
    }

    pub fn pixels_mut(&self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }

    pub fn to_image(&self) -> Image {
        unsafe { GdiFlush() };
        let mut px = self.pixels_mut().to_vec();
        for p in px.iter_mut() {
            *p |= 0xFF00_0000;
        }
        Image { w: self.w, h: self.h, px }
    }

    pub fn copy_from(&self, other: &Dib) {
        unsafe { GdiFlush() };
        self.pixels_mut().copy_from_slice(other.pixels_mut());
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Put the image on the clipboard as a 24-bit CF_DIB (the most widely accepted format).
pub fn copy_to_clipboard(owner: HWND, img: &Image) -> bool {
    unsafe {
        let stride = ((img.w * 3 + 3) & !3) as usize;
        let hdr = std::mem::size_of::<BITMAPINFOHEADER>();
        let size = hdr + stride * img.h as usize;
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, size) else { return false };
        let p = GlobalLock(hmem) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(hmem));
            return false;
        }
        let header = BITMAPINFOHEADER {
            biSize: hdr as u32,
            biWidth: img.w,
            biHeight: img.h, // bottom-up
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            biSizeImage: (stride * img.h as usize) as u32,
            ..Default::default()
        };
        std::ptr::copy_nonoverlapping(&header as *const _ as *const u8, p, hdr);
        let data = std::slice::from_raw_parts_mut(p.add(hdr), stride * img.h as usize);
        for y in 0..img.h as usize {
            let src = &img.px[y * img.w as usize..(y + 1) * img.w as usize];
            let dst_row = (img.h as usize - 1 - y) * stride;
            for (x, &c) in src.iter().enumerate() {
                let o = dst_row + x * 3;
                data[o] = (c & 0xFF) as u8;
                data[o + 1] = ((c >> 8) & 0xFF) as u8;
                data[o + 2] = ((c >> 16) & 0xFF) as u8;
            }
        }
        let _ = GlobalUnlock(hmem);

        // Another app may briefly hold the clipboard; retry a few times.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(Some(owner)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        if !opened {
            let _ = GlobalFree(Some(hmem));
            return false;
        }
        let _ = EmptyClipboard();
        let ok = SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(hmem.0))).is_ok();
        let _ = CloseClipboard();
        if !ok {
            let _ = GlobalFree(Some(hmem));
        }
        ok
    }
}

// ---------------------------------------------------------------------------
// DPI, fonts, text, window chrome
// ---------------------------------------------------------------------------

pub fn dpi_for_window(hwnd: HWND) -> u32 {
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 { 96 } else { d }
}

pub fn dpi_for_point(x: i32, y: i32) -> u32 {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dx = 96u32;
        let mut dy = 96u32;
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        dx
    }
}

/// Scale a 96-DPI design value.
pub fn sc(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}
pub fn scf(v: f32, dpi: u32) -> f32 {
    v * dpi as f32 / 96.0
}

pub fn ui_font(px: i32, weight: i32) -> HFONT {
    let face = WStr::new("Segoe UI");
    unsafe {
        CreateFontW(
            -px, 0, 0, 0, weight, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY, (DEFAULT_PITCH.0 | FF_SWISS.0) as u32, face.p(),
        )
    }
}

pub fn draw_text(hdc: HDC, text: &str, rc: RECT, font: HFONT, color: u32, flags: DRAW_TEXT_FORMAT) {
    unsafe {
        let old = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, rgb_from_argb(color));
        let mut w: Vec<u16> = text.encode_utf16().collect();
        let mut r = rc;
        DrawTextW(hdc, &mut w, &mut r, flags);
        SelectObject(hdc, old);
    }
}

pub fn text_width(hdc: HDC, text: &str, font: HFONT) -> i32 {
    unsafe {
        let old = SelectObject(hdc, font.into());
        let w: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &w, &mut sz);
        SelectObject(hdc, old);
        sz.cx
    }
}

pub fn rgb_from_argb(c: u32) -> COLORREF {
    let r = (c >> 16) & 0xFF;
    let g = (c >> 8) & 0xFF;
    let b = c & 0xFF;
    COLORREF(r | (g << 8) | (b << 16))
}

/// Dark title bar + rounded corners (Windows 11; silently ignored on Windows 10).
pub fn modern_chrome(hwnd: HWND, dark_title: bool, round: bool) {
    unsafe {
        if dark_title {
            let on = BOOL(1);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &on as *const _ as *const _,
                std::mem::size_of::<BOOL>() as u32,
            );
        }
        if round {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
        }
    }
}

pub fn pt_in(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

pub fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

pub fn work_area_for_point(x: i32, y: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        mi.rcWork
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn hinstance() -> HINSTANCE {
    unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

pub fn app_icon(size: i32) -> HICON {
    unsafe {
        LoadImageW(Some(hinstance()), PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    }
}

/// Store / fetch a boxed per-window state pointer in GWLP_USERDATA.
pub unsafe fn set_state<T>(hwnd: HWND, state: Box<T>) {
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
}
pub unsafe fn get_state<'a, T>(hwnd: HWND) -> Option<&'a mut T> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
    if p.is_null() { None } else { Some(&mut *p) }
}
pub unsafe fn take_state<T>(hwnd: HWND) -> Option<Box<T>> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    if p.is_null() { None } else { Some(Box::from_raw(p)) }
}

pub fn register_class(name: &WStr, proc_: WNDPROC, cursor: PCWSTR, style: WNDCLASS_STYLES) {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style,
            lpfnWndProc: proc_,
            hInstance: hinstance(),
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            hIcon: app_icon(32),
            hIconSm: app_icon(16),
            lpszClassName: name.p(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}
