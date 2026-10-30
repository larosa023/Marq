//! Win+Shift+S without fighting Windows for it.
//!
//! Explorer owns Win+Shift+S (even with Snipping Tool uninstalled), so RegisterHotKey
//! can't claim it. Like PowerToys, we watch the keyboard with a low-level hook, swallow
//! the S keystroke when Win+Shift are held, and tell the app to start a snip.
//!
//! The hook lives on its own thread with its own message loop, so nothing the app does
//! (capturing the screen, copying to the clipboard) can ever delay anyone's typing or
//! make Windows drop the hook for being slow.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static TARGET: AtomicIsize = AtomicIsize::new(0); // HWND to notify
static MESSAGE: AtomicIsize = AtomicIsize::new(0); // message to post
static SWALLOW_S_UP: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

const LLKHF_INJECTED: u32 = 0x10;
/// An unassigned virtual-key code. Tapping it tells Explorer "another key was pressed
/// with Win", so letting go of Win doesn't pop open the Start menu.
const VK_DUMMY: u16 = 0xFF;

/// Start the hook thread. Returns once we know whether the hook installed.
pub fn install(notify: HWND, msg: u32) -> bool {
    TARGET.store(notify.0 as isize, Ordering::Relaxed);
    MESSAGE.store(msg as isize, Ordering::Relaxed);
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

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let injected = kb.flags.0 & LLKHF_INJECTED != 0;
        let msg = wp.0 as u32;
        if !injected && kb.vkCode == VK_S.0 as u32 {
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if is_down
                && (down(VK_LWIN) || down(VK_RWIN))
                && down(VK_SHIFT)
                && !down(VK_CONTROL)
                && !down(VK_MENU)
            {
                // Keep Start from opening when Win is released. Sending input from our
                // process also lets the snip overlay take focus.
                let key = |flags| INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_DUMMY), dwFlags: flags, ..Default::default() },
                    },
                };
                SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);

                SWALLOW_S_UP.store(true, Ordering::Relaxed);
                let target = HWND(TARGET.load(Ordering::Relaxed) as *mut _);
                let m = MESSAGE.load(Ordering::Relaxed) as u32;
                let _ = PostMessageW(Some(target), m, WPARAM(0), LPARAM(0));
                return LRESULT(1); // swallow: Windows never sees Win+Shift+S
            }
            if is_up && SWALLOW_S_UP.swap(false, Ordering::Relaxed) {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}
