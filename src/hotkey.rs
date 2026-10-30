//! The one snip shortcut (Win+Shift+S by default, changeable in Settings).
//!
//! Explorer owns Win+Shift+S (even with Snipping Tool uninstalled), so RegisterHotKey
//! can't claim it. Like PowerToys, we watch the keyboard with a low-level hook, swallow
//! the shortcut's key when the right modifiers are held, and tell the app to snip.
//! The same hook records a new shortcut while the Settings window is listening.
//!
//! The hook lives on its own thread with its own message loop, so nothing the app does
//! (capturing the screen, copying to the clipboard) can ever delay anyone's typing or
//! make Windows drop the hook for being slow.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const M_WIN: u32 = 1;
pub const M_SHIFT: u32 = 2;
pub const M_CTRL: u32 = 4;
pub const M_ALT: u32 = 8;
pub const DEFAULT: (u32, u32) = (M_WIN | M_SHIFT, 0x53); // Win+Shift+S

/// Posted to the Settings window while recording: wParam = mods, lParam = vk.
/// wParam = u32::MAX means the user pressed Esc to cancel.
pub const RECORD_CANCELLED: usize = u32::MAX as usize;

static MODS: AtomicU32 = AtomicU32::new(DEFAULT.0);
static VK: AtomicU32 = AtomicU32::new(DEFAULT.1);
static TARGET: AtomicIsize = AtomicIsize::new(0); // app window to notify on trigger
static MESSAGE: AtomicU32 = AtomicU32::new(0);
static REC_TARGET: AtomicIsize = AtomicIsize::new(0); // non-zero while recording
static REC_MESSAGE: AtomicU32 = AtomicU32::new(0);
static SWALLOW_UP: AtomicU32 = AtomicU32::new(0); // key whose release we also hide
static INSTALLED: AtomicBool = AtomicBool::new(false);

const LLKHF_INJECTED: u32 = 0x10;
/// An unassigned virtual-key code. Tapping it tells Windows "another key was pressed",
/// so letting go of Win doesn't open Start and letting go of Alt doesn't open menus.
const VK_DUMMY: u16 = 0xFF;

pub fn set(mods: u32, vk: u32) {
    MODS.store(mods, Ordering::Relaxed);
    VK.store(vk, Ordering::Relaxed);
}

pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

pub fn start_recording(hwnd: HWND, msg: u32) {
    REC_MESSAGE.store(msg, Ordering::Relaxed);
    REC_TARGET.store(hwnd.0 as isize, Ordering::Relaxed);
}

pub fn stop_recording() {
    REC_TARGET.store(0, Ordering::Relaxed);
}

/// Start the hook thread. Returns once we know whether the hook installed.
pub fn install(notify: HWND, msg: u32) -> bool {
    TARGET.store(notify.0 as isize, Ordering::Relaxed);
    MESSAGE.store(msg, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(crate::util::hinstance()), 0);
        let ok = hook.is_ok();
        INSTALLED.store(ok, Ordering::Relaxed);
        let _ = tx.send(ok);
        if !ok {
            return;
        }
        // Low-level hooks are called through this thread's message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Ok(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
}

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

fn is_modifier(vk: u32) -> bool {
    [
        VK_LWIN, VK_RWIN, VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL,
        VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU,
    ]
    .iter()
    .any(|k| k.0 as u32 == vk)
}

fn current_mods() -> u32 {
    let mut m = 0;
    if down(VK_LWIN) || down(VK_RWIN) {
        m |= M_WIN;
    }
    if down(VK_SHIFT) {
        m |= M_SHIFT;
    }
    if down(VK_CONTROL) {
        m |= M_CTRL;
    }
    if down(VK_MENU) {
        m |= M_ALT;
    }
    m
}

unsafe fn tap_dummy() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_DUMMY), dwFlags: flags, ..Default::default() },
        },
    };
    SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
}

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let msg = wp.0 as u32;
        let vk = kb.vkCode;
        let injected = kb.flags.0 & LLKHF_INJECTED != 0;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        if !injected && !is_modifier(vk) {
            if is_up && SWALLOW_UP.load(Ordering::Relaxed) == vk {
                SWALLOW_UP.store(0, Ordering::Relaxed);
                return LRESULT(1);
            }
            if is_down {
                let mods = current_mods();
                let rec = REC_TARGET.load(Ordering::Relaxed);
                let hit = rec == 0 && vk == VK.load(Ordering::Relaxed) && mods == MODS.load(Ordering::Relaxed);
                if rec != 0 || hit {
                    if mods & (M_WIN | M_ALT) != 0 {
                        tap_dummy();
                    }
                    SWALLOW_UP.store(vk, Ordering::Relaxed);
                    if rec != 0 {
                        let cancel = vk == VK_ESCAPE.0 as u32 && mods == 0;
                        let (w, l) = if cancel { (RECORD_CANCELLED, 0) } else { (mods as usize, vk as isize) };
                        let _ = PostMessageW(
                            Some(HWND(rec as *mut _)),
                            REC_MESSAGE.load(Ordering::Relaxed),
                            WPARAM(w),
                            LPARAM(l),
                        );
                    } else {
                        let _ = PostMessageW(
                            Some(HWND(TARGET.load(Ordering::Relaxed) as *mut _)),
                            MESSAGE.load(Ordering::Relaxed),
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                    return LRESULT(1); // swallow: Windows and apps never see it
                }
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

// ---------------------------------------------------------------------------
// Names and validation
// ---------------------------------------------------------------------------

pub fn key_name(vk: u32) -> String {
    let v = vk as u16;
    match v {
        0x30..=0x39 | 0x41..=0x5A => (v as u8 as char).to_string(),
        _ if (VK_F1.0..=VK_F24.0).contains(&v) => format!("F{}", v - VK_F1.0 + 1),
        _ if v == VK_SNAPSHOT.0 => "PrtScn".into(),
        _ if v == VK_PAUSE.0 => "Pause".into(),
        _ if v == VK_SCROLL.0 => "ScrollLock".into(),
        _ if v == VK_SPACE.0 => "Space".into(),
        _ if v == VK_RETURN.0 => "Enter".into(),
        _ if v == VK_TAB.0 => "Tab".into(),
        _ if v == VK_ESCAPE.0 => "Esc".into(),
        _ if v == VK_BACK.0 => "Backspace".into(),
        _ if v == VK_INSERT.0 => "Insert".into(),
        _ if v == VK_DELETE.0 => "Delete".into(),
        _ if v == VK_HOME.0 => "Home".into(),
        _ if v == VK_END.0 => "End".into(),
        _ if v == VK_PRIOR.0 => "PgUp".into(),
        _ if v == VK_NEXT.0 => "PgDn".into(),
        _ if v == VK_LEFT.0 => "Left".into(),
        _ if v == VK_RIGHT.0 => "Right".into(),
        _ if v == VK_UP.0 => "Up".into(),
        _ if v == VK_DOWN.0 => "Down".into(),
        _ => unsafe {
            let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            let mut buf = [0u16; 32];
            let n = GetKeyNameTextW((sc << 16) as i32, &mut buf);
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                format!("Key {vk}")
            }
        },
    }
}

pub fn label(mods: u32, vk: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if mods & M_WIN != 0 {
        parts.push("Win".into());
    }
    if mods & M_CTRL != 0 {
        parts.push("Ctrl".into());
    }
    if mods & M_ALT != 0 {
        parts.push("Alt".into());
    }
    if mods & M_SHIFT != 0 {
        parts.push("Shift".into());
    }
    parts.push(key_name(vk));
    parts.join(" + ")
}

/// Why a shortcut can't be used, if it can't.
pub fn problem(mods: u32, vk: u32) -> Option<&'static str> {
    let v = vk as u16;
    let standalone = (VK_F1.0..=VK_F24.0).contains(&v)
        || v == VK_SNAPSHOT.0
        || v == VK_PAUSE.0
        || v == VK_SCROLL.0;
    if mods & (M_WIN | M_CTRL | M_ALT) == 0 && !standalone {
        return Some("Add Win, Ctrl or Alt so it doesn't get in the way of typing.");
    }
    let reserved = (mods == M_CTRL && b"CVXZYA".contains(&(v as u8)) && v < 0x80)
        || (mods == M_ALT && (v == VK_TAB.0 || v == VK_F4.0 || v == VK_ESCAPE.0))
        || (mods == M_WIN && v == 0x4C); // Win+L
    if reserved {
        return Some("That one is already used by Windows. Try another.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rules() {
        assert_eq!(label(M_WIN | M_SHIFT, 0x53), "Win + Shift + S");
        assert!(problem(M_WIN | M_SHIFT, 0x53).is_none());
        assert!(problem(0, VK_SNAPSHOT.0 as u32).is_none());
        assert!(problem(M_SHIFT, 0x41).is_some());
        assert!(problem(M_CTRL, 0x43).is_some());
        assert!(problem(M_CTRL | M_SHIFT, 0x43).is_none());
    }
}
