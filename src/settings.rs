//! "Start with Windows" via the per-user Run key (no admin needed).

use windows::Win32::System::Registry::*;

use crate::util::WStr;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE: &str = "Marq";

fn open_run_key() -> Option<HKEY> {
    let key = WStr::new(RUN_KEY);
    let mut hkey = HKEY::default();
    let r = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, key.p(), Some(0), KEY_READ | KEY_WRITE, &mut hkey)
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

/// True unless the user turned off Windows' "Use the Print screen key to open screen
/// capture" (HKCU\Control Panel\Keyboard\PrintScreenKeyForSnippingEnabled = 0).
/// On current Windows 11 builds it's on by default even when the value is absent.
pub fn windows_uses_prtscn() -> bool {
    let key = WStr::new("Control Panel\\Keyboard");
    let name = WStr::new("PrintScreenKeyForSnippingEnabled");
    let mut data: u32 = 1;
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
    !(r.is_ok() && data == 0)
}
