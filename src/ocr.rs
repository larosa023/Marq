//! Copy text out of a snip using Windows' built-in OCR engine (Windows.Media.Ocr).
//! Nothing is downloaded or sent anywhere; recognition runs locally, on a background
//! thread, in the languages installed on the PC.

use std::sync::Mutex;

use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::util::{Dib, Image};

pub struct Done {
    pub result: Result<String, String>,
    pub near: RECT,
}

static RESULTS: Mutex<Vec<Done>> = Mutex::new(Vec::new());

pub fn take_results() -> Vec<Done> {
    RESULTS.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
}

/// Recognize in the background; posts `msg` to `hwnd` when a result is ready.
pub fn recognize_async(img: Image, near: RECT, hwnd: HWND, msg: u32) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || {
        // No CoInitialize/CoUninitialize here on purpose: the windows crate caches WinRT
        // factories for the life of the process and keeps the MTA alive itself. Tearing
        // COM down after each snip would leave those cached pointers dangling.
        let result = recognize(&img);
        if let Ok(mut v) = RESULTS.lock() {
            v.push(Done { result, near });
        }
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    });
}

fn recognize(img: &Image) -> Result<String, String> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|_| "No text-recognition language installed.\nAdd one in Settings \u{203A} Time & language.".to_string())?;

    // Screen text is small; OCR reads it far better enlarged. Stay within the engine's limit.
    // Only enlarge small crops, and keep the result modest so big selections stay fast.
    let max = OcrEngine::MaxImageDimension().unwrap_or(2600) as i32;
    let cap = max.min(4000);
    let longest = img.w.max(img.h);
    let area = img.w as i64 * img.h as i64;
    let factor = [3, 2]
        .into_iter()
        .find(|&f| longest * f <= cap && area * (f * f) as i64 <= 8_000_000)
        .unwrap_or(1);
    let (w, h, px) = if longest > max {
        let s = max as f32 / longest as f32;
        let (tw, th) = (((img.w as f32 * s) as i32).max(1), ((img.h as f32 * s) as i32).max(1));
        scale(img, tw, th).ok_or_else(|| "Couldn't prepare the image.".to_string())?
    } else if factor > 1 {
        scale(img, img.w * factor, img.h * factor).unwrap_or_else(|| (img.w, img.h, img.px.clone()))
    } else {
        (img.w, img.h, img.px.clone())
    };

    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
    let fail = |_| "Couldn't read text from that area.".to_string();
    let writer = DataWriter::new().map_err(fail)?;
    writer.WriteBytes(bytes).map_err(fail)?;
    let buffer = writer.DetachBuffer().map_err(fail)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w, h).map_err(fail)?;
    let result = engine.RecognizeAsync(&bitmap).map_err(fail)?.get().map_err(fail)?;

    let mut lines: Vec<String> = Vec::new();
    for line in result.Lines().map_err(fail)? {
        if let Ok(t) = line.Text() {
            let t = t.to_string_lossy();
            if !t.trim().is_empty() {
                lines.push(t);
            }
        }
    }
    if lines.is_empty() {
        return Err("No text found in that area.".into());
    }
    Ok(lines.join("\r\n"))
}

/// Smooth resize with GDI's HALFTONE filter (works on any thread).
fn scale(img: &Image, w: i32, h: i32) -> Option<(i32, i32, Vec<u32>)> {
    let src = Dib::from_image(img)?;
    let dst = Dib::new(w, h)?;
    unsafe {
        SetStretchBltMode(dst.dc, HALFTONE);
        let _ = SetBrushOrgEx(dst.dc, 0, 0, None);
        let _ = StretchBlt(dst.dc, 0, 0, w, h, Some(src.dc), 0, 0, src.w, src.h, SRCCOPY);
        let _ = GdiFlush();
    }
    let mut px = dst.pixels_mut().to_vec();
    for p in px.iter_mut() {
        *p |= 0xFF00_0000;
    }
    Some((w, h, px))
}
