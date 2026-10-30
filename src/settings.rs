//! Saved settings: "Start with Windows" (per-user Run key, no admin needed) and the
//! snip shortcut (HKCU\Software\Marq).

use windows::Win32::System::Registry::*;

use crate::util::WStr;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE: &str = "Marq";

fn open_run_key() -> Option<HKEY> {
    let key = WStr::new(RUN_KEY);
    let mut hkey = HKEY::default();
    // Create-or-open: on a fresh profile the Run key may not exist yet.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.p(),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if r.is_ok() { Some(hkey) } else { None }
}

pub fn autostart_enabled() -> bool {
    let Some(hkey) = open_run_key() else { return false };
    let name = WStr::new(VALUE);
    let r = unsafe { RegQueryValueExW(hkey, name.p(), None, None, None, None) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    r.is_ok()
}

pub fn set_autostart(on: bool) {
    let Some(hkey) = open_run_key() else { return };
    let name = WStr::new(VALUE);
    unsafe {
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = format!("\"{}\" --autostart", exe.display());
                let data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
                let _ = RegSetValueExW(hkey, name.p(), Some(0), REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(hkey, name.p());
        }
        let _ = RegCloseKey(hkey);
    }
}

/// If autostart is on, make sure it still points at this exe (e.g. after moving it).
pub fn refresh_autostart_path() {
    if autostart_enabled() {
        set_autostart(true);
    }
}

const APP_KEY: &str = "Software\\Marq";

fn read_dword(name: &str) -> Option<u32> {
    let key = WStr::new(APP_KEY);
    let name = WStr::new(name);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.p(),
            name.p(),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    if r.is_ok() { Some(data) } else { None }
}

fn write_dword(hkey: HKEY, name: &str, v: u32) {
    let name = WStr::new(name);
    unsafe {
        let _ = RegSetValueExW(hkey, name.p(), Some(0), REG_DWORD, Some(&v.to_le_bytes()));
    }
}

/// The snip shortcut as (modifier bits, virtual key). Defaults to Win+Shift+S.
pub fn load_hotkey() -> (u32, u32) {
    match (read_dword("HotkeyMods"), read_dword("HotkeyKey")) {
        (Some(m), Some(k)) if k != 0 && crate::hotkey::problem(m, k).is_none() => (m, k),
        _ => crate::hotkey::DEFAULT,
    }
}

pub fn save_hotkey(mods: u32, vk: u32) {
    let key = WStr::new(APP_KEY);
    let mut hkey = HKEY::default();
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.p(),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    if r.is_ok() {
        write_dword(hkey, "HotkeyMods", mods);
        write_dword(hkey, "HotkeyKey", vk);
        unsafe {
            let _ = RegCloseKey(hkey);
        }
    }
}
