//! Save snips as PNGs to Pictures\Screenshots, the folder Windows' own screenshots use,
//! so they show up in File Explorer's Gallery and the Photos app.
//!
//! PNG encoding uses GDI+ (built into Windows), on a background thread, so the snip
//! is on the clipboard instantly and saving never adds any delay.

use std::path::PathBuf;
use std::sync::Mutex;

use windows::core::GUID;
use windows::Win32::Graphics::GdiPlus::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::*;

use crate::util::WStr;

const PIXEL_FORMAT_32BPP_RGB: i32 = 0x0002_2009;
/// GDI+'s built-in PNG encoder.
const PNG_ENCODER: GUID = GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);

/// One save at a time, in order: re-saves of a marked-up snip land after the original.
static SAVING: Mutex<()> = Mutex::new(());

/// The Screenshots folder (created if needed). Follows OneDrive/folder redirection.
pub fn folder() -> Option<PathBuf> {
    unsafe {
        let known = |id: &GUID| -> Option<PathBuf> {
            let p = SHGetKnownFolderPath(id, KF_FLAG_CREATE, None).ok()?;
            let path = p.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(p.0 as *const _));
            path
        };
        let dir = known(&FOLDERID_Screenshots)
            .or_else(|| known(&FOLDERID_Pictures).map(|p| p.join("Screenshots")))?;
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }
}

/// A fresh file name like "Screenshot 2026-10-30 133512.png" (same style as Windows).
pub fn new_path() -> Option<PathBuf> {
    let dir = folder()?;
    let t = unsafe { GetLocalTime() };
    let stem = format!(
        "Screenshot {:04}-{:02}-{:02} {:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let mut path = dir.join(format!("{stem}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).png"));
        n += 1;
    }
    Some(path)
}

/// Write (or overwrite) `path` with these pixels, in the background.
pub fn save_async(px: Vec<u32>, w: i32, h: i32, path: PathBuf) {
    std::thread::spawn(move || {
        let _guard = SAVING.lock();
        let _ = save_png(&px, w, h, &path);
    });
}

fn save_png(px: &[u32], w: i32, h: i32, path: &PathBuf) -> bool {
    if w <= 0 || h <= 0 || px.len() < (w as usize) * (h as usize) {
        return false;
    }
    // Write to a temp name first so the Gallery never shows a half-written file.
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    unsafe {
        let mut bmp: *mut GpBitmap = std::ptr::null_mut();
        GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            PIXEL_FORMAT_32BPP_RGB,
            Some(px.as_ptr() as *const u8),
            &mut bmp,
        );
        if bmp.is_null() {
            return false;
        }
        let name = WStr::new(&tmp.to_string_lossy());
        let status = GdipSaveImageToFile(bmp as *mut GpImage, name.p(), &PNG_ENCODER, std::ptr::null());
        GdipDisposeImage(bmp as *mut GpImage);
        if status != Ok {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

pub fn open_folder() {
    if let Some(dir) = folder() {
        let p = WStr::new(&dir.to_string_lossy());
        let verb = WStr::new("open");
        unsafe {
            ShellExecuteW(None, verb.p(), p.p(), None, None, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
        }
    }
}
