//! Marq — a hyper-lightweight screen snipping tool for Windows.
//! Lives in the system tray; snips go straight to the clipboard.

#![windows_subsystem = "windows"]
// A misspelled/unimported constant in a `match` silently becomes a catch-all binding.
// Make that a hard error instead of a warning.
#![deny(unreachable_patterns, unused_variables)]

mod capture;
mod editor;
mod gallery;
mod gfx;
mod hotkey;
mod overlay;
mod settings;
mod settings_ui;
mod toast;
mod updater;
mod util;

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use overlay::Mode;
use updater::{CheckResult, Event, InstallResult};
use util::*;

const HOST_CLASS: &str = "MarqHost";
const MUTEX_NAME: &str = "Local\\MarqSingleInstance-7f3a";

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SNIP: u32 = WM_APP + 2; // from a second instance; wParam = mode
const WM_APP_UPDATER: u32 = WM_APP + 3; // updater::take_events() has results
const WM_APP_SNIP_KEY: u32 = WM_APP + 4; // the snip shortcut was pressed (hook thread)

// NIN_SELECT | NINF_KEY (not exported by the bindings).
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;

/// Only used if the keyboard hook can't be installed (very rare).
const HK_FALLBACK: i32 = 1;

const TIMER_SNIP: usize = 1;
const TIMER_UPDATE_FIRST: usize = 2;
const TIMER_UPDATE_PERIODIC: usize = 3;
const TIMER_TRIM: usize = 4;
const TIMER_TRAY_RETRY: usize = 5;
const FIRST_CHECK_MS: u32 = 10_000;
const PERIODIC_CHECK_MS: u32 = 6 * 60 * 60 * 1000;

const ID_REGION: usize = 100;
const ID_WINDOW: usize = 101;
const ID_SCREEN: usize = 102;
const ID_SETTINGS: usize = 110;
const ID_UPDATE: usize = 111;
const ID_OPEN_RELEASES: usize = 112;
const ID_OPEN_FOLDER: usize = 113;
const ID_QUIT: usize = 120;

static HOST: AtomicIsize = AtomicIsize::new(0);

#[derive(PartialEq)]
enum Balloon {
    None,
    UpdateAvailable,
}

struct App {
    taskbar_created: u32,
    pending_snip: Mode,
    hotkey_label: String,
    update: Option<(String, String)>, // (version, sha256)
    notified_version: Option<String>,
    updating: bool,
    balloon: Balloon,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App {
        taskbar_created: 0,
        pending_snip: Mode::Region,
        hotkey_label: String::new(),
        update: None,
        notified_version: None,
        updating: false,
        balloon: Balloon::None,
    });
}

pub fn host() -> HWND {
    HWND(HOST.load(Ordering::Relaxed) as *mut _)
}

/// Called by the overlay with the finished snip.
pub fn on_snip(img: Image, screen_rect: RECT) {
    let copied = copy_to_clipboard(host(), &img.px, img.w, img.h);
    // Also keep it in Pictures\Screenshots so it shows up in the Gallery.
    let saved = if settings::save_to_gallery() { gallery::new_path() } else { None };
    if let Some(p) = &saved {
        gallery::save_async(img.px.clone(), img.w, img.h, p.clone());
    }
    toast::show(img, screen_rect, copied, saved);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let updated = args.iter().any(|a| a == "--updated");
    let autostarted = args.iter().any(|a| a == "--autostart");

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Single instance. Launching Marq again just starts a snip in the running copy.
        let name = WStr::new(MUTEX_NAME);
        let Ok(mutex) = CreateMutexW(None, false, name.p()) else { return };
        let wait_ms = if updated { 30_000 } else { 0 };
        let w = WaitForSingleObject(mutex, wait_ms);
        if w != WAIT_OBJECT_0 && w != WAIT_ABANDONED {
            let cls = WStr::new(HOST_CLASS);
            if let Ok(other) = FindWindowW(cls.p(), None) {
                // We were just launched by the user, so we may pass on foreground rights.
                let mut pid = 0u32;
                GetWindowThreadProcessId(other, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                let _ = PostMessageW(Some(other), WM_APP_SNIP, WPARAM(0), LPARAM(0));
            }
            return;
        }

        gfx::startup();
        overlay::register();
        toast::register();
        editor::register();
        settings_ui::register();
        register_class(&WStr::new(HOST_CLASS), Some(host_proc), IDC_ARROW, WNDCLASS_STYLES(0));

        let cls = WStr::new(HOST_CLASS);
        let title = WStr::new("Marq");
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            cls.p(),
            title.p(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance()),
            None,
        ) else {
            return;
        };
        HOST.store(hwnd.0 as isize, Ordering::Relaxed);

        let tb = WStr::new("TaskbarCreated");
        let taskbar_created = RegisterWindowMessageW(tb.p());
        let hotkey_label = register_hotkeys(hwnd);
        APP.with(|a| {
            let mut a = a.borrow_mut();
            a.taskbar_created = taskbar_created;
            a.hotkey_label = hotkey_label;
        });

        if !add_tray_icon(hwnd) {
            SetTimer(Some(hwnd), TIMER_TRAY_RETRY, 2_000, None);
        }
        updater::cleanup_old();
        settings::refresh_autostart_path();

        if updated {
            balloon(hwnd, "Marq updated", &format!("You're now on v{}.", updater::CURRENT), Balloon::None);
        } else if !autostarted {
            let hk = APP.with(|a| a.borrow().hotkey_label.clone());
            balloon(
                hwnd,
                "Marq is running",
                &format!("Press {hk} or click the tray icon to snip."),
                Balloon::None,
            );
        }

        SetTimer(Some(hwnd), TIMER_UPDATE_FIRST, FIRST_CHECK_MS, None);
        SetTimer(Some(hwnd), TIMER_TRIM, 3_000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = ReleaseMutex(mutex);
    }
}

/// Install the one snip shortcut (saved in Settings; Win+Shift+S by default).
fn register_hotkeys(hwnd: HWND) -> String {
    let (mods, vk) = settings::load_hotkey();
    hotkey::set(mods, vk);
    hotkey::install(hwnd, WM_APP_SNIP_KEY);
    register_fallback(hwnd, mods, vk);
    hotkey::label(mods, vk)
}

/// If the hook couldn't install, fall back to a normal hotkey (works for most combos
/// that don't involve the Win key).
fn register_fallback(hwnd: HWND, mods: u32, vk: u32) {
    if hotkey::installed() {
        return;
    }
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HK_FALLBACK);
        let mut m = MOD_NOREPEAT;
        if mods & hotkey::M_WIN != 0 {
            m |= MOD_WIN;
        }
        if mods & hotkey::M_SHIFT != 0 {
            m |= MOD_SHIFT;
        }
        if mods & hotkey::M_CTRL != 0 {
            m |= MOD_CONTROL;
        }
        if mods & hotkey::M_ALT != 0 {
            m |= MOD_ALT;
        }
        let _ = RegisterHotKey(Some(hwnd), HK_FALLBACK, m, vk);
    }
}

/// Called by the Settings window after the user picks a new shortcut.
pub fn apply_hotkey(mods: u32, vk: u32) {
    hotkey::set(mods, vk);
    let hwnd = host();
    register_fallback(hwnd, mods, vk);
    APP.with(|a| a.borrow_mut().hotkey_label = hotkey::label(mods, vk));
    update_tray_tip(hwnd);
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    nid
}

fn tray_tip() -> String {
    let hk = APP.with(|a| a.borrow().hotkey_label.clone());
    format!("Marq \u{2014} {hk} to snip")
}

fn update_tray_tip(hwnd: HWND) {
    unsafe {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_TIP | NIF_SHOWTIP;
        fill_wide(&mut nid.szTip, &tray_tip());
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Add the tray icon. Returns false if Explorer wasn't ready (common right at login).
fn add_tray_icon(hwnd: HWND) -> bool {
    unsafe {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        let dpi = dpi_for_window(hwnd);
        nid.hIcon = app_icon(GetSystemMetricsForDpi(SM_CXSMICON, dpi));
        fill_wide(&mut nid.szTip, &tray_tip());
        let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
            // Already there (e.g. TaskbarCreated raced us): just refresh it.
            || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
        if ok {
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        ok
    }
}

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let nid = tray_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn balloon(hwnd: HWND, title: &str, text: &str, kind: Balloon) {
    APP.with(|a| a.borrow_mut().balloon = kind);
    unsafe {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_INFO;
        fill_wide(&mut nid.szInfoTitle, title);
        fill_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        nid.hBalloonIcon = app_icon(GetSystemMetrics(SM_CXICON));
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Start a snip after a short delay so menus / tray flyouts have faded out first.
fn snip_soon(hwnd: HWND, mode: Mode, delay_ms: u32) {
    APP.with(|a| a.borrow_mut().pending_snip = mode);
    unsafe {
        SetTimer(Some(hwnd), TIMER_SNIP, delay_ms, None);
    }
}

fn start_install(hwnd: HWND) {
    let job = APP.with(|a| {
        let mut a = a.borrow_mut();
        if a.updating {
            return None;
        }
        let u = a.update.clone();
        if u.is_some() {
            a.updating = true;
        }
        u
    });
    if let Some((version, sha)) = job {
        balloon(hwnd, "Updating Marq", &format!("Downloading v{version}\u{2026}"), Balloon::None);
        updater::install_async(hwnd, WM_APP_UPDATER, version, sha);
    }
}

fn show_menu(hwnd: HWND, anchor: (i32, i32)) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |id: usize, text: &str, flags: MENU_ITEM_FLAGS| {
            let t = WStr::new(text);
            let _ = AppendMenuW(menu, flags, id, t.p());
        };
        let (hk, update, updating) = APP.with(|a| {
            let a = a.borrow();
            (a.hotkey_label.clone(), a.update.clone(), a.updating)
        });
        let first_key = hk.replace(" + ", "+");

        add(0, &format!("Marq v{}", updater::CURRENT), MF_STRING | MF_GRAYED);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add(ID_REGION, &format!("Snip region\t{first_key}"), MF_STRING);
        add(ID_WINDOW, "Snip window", MF_STRING);
        add(ID_SCREEN, "Snip full screen", MF_STRING);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add(ID_SETTINGS, "Settings\u{2026}", MF_STRING);
        match (&update, updating) {
            (_, true) => add(ID_UPDATE, "Updating\u{2026}", MF_STRING | MF_GRAYED),
            (Some((v, _)), false) => add(ID_UPDATE, &format!("Update to v{v}"), MF_STRING),
            (None, false) => add(ID_UPDATE, "Check for updates", MF_STRING),
        }
        add(ID_OPEN_FOLDER, "Open Screenshots folder", MF_STRING);
        add(ID_OPEN_RELEASES, "What's new\u{2026}", MF_STRING);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add(ID_QUIT, "Quit", MF_STRING);
        if update.is_some() && !updating {
            let _ = SetMenuDefaultItem(menu, ID_UPDATE as u32, 0);
        }

        let (x, y) = anchor;
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_BOTTOMALIGN,
            x,
            y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            ID_REGION => snip_soon(hwnd, Mode::Region, 200),
            ID_WINDOW => snip_soon(hwnd, Mode::Window, 200),
            ID_SCREEN => snip_soon(hwnd, Mode::Screen, 200),
            ID_SETTINGS => settings_ui::open(),
            ID_UPDATE => {
                if APP.with(|a| a.borrow().update.is_some()) {
                    start_install(hwnd);
                } else {
                    updater::check_async(hwnd, WM_APP_UPDATER, true);
                }
            }
            ID_OPEN_FOLDER => gallery::open_folder(),
            ID_OPEN_RELEASES => {
                let url = WStr::new(&format!(
                    "https://github.com/{}/{}/releases",
                    updater::REPO_OWNER,
                    updater::REPO_NAME
                ));
                let verb = WStr::new("open");
                ShellExecuteW(None, verb.p(), url.p(), None, None, SW_SHOWNORMAL);
            }
            ID_QUIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

fn on_checked(hwnd: HWND, manual: bool, res: CheckResult) {
    match res {
        CheckResult::Available { version, sha256 } => {
            let first_time = APP.with(|a| {
                let mut a = a.borrow_mut();
                a.update = Some((version.clone(), sha256));
                let first = a.notified_version.as_deref() != Some(version.as_str());
                a.notified_version = Some(version.clone());
                first
            });
            if manual {
                start_install(hwnd);
            } else if first_time {
                balloon(
                    hwnd,
                    &format!("Marq v{version} is available"),
                    "Click here to update, or use the tray menu.",
                    Balloon::UpdateAvailable,
                );
            }
        }
        CheckResult::UpToDate => {
            APP.with(|a| a.borrow_mut().update = None);
            if manual {
                balloon(
                    hwnd,
                    "You're up to date",
                    &format!("Marq v{} is the latest version.", updater::CURRENT),
                    Balloon::None,
                );
            }
        }
        CheckResult::Failed(e) => {
            if manual {
                balloon(hwnd, "Couldn't check for updates", &e, Balloon::None);
            }
        }
    }
}

fn on_installed(hwnd: HWND, res: InstallResult) {
    match res {
        InstallResult::Ready(exe) => {
            // Hand over to the new version; it waits for this process to exit.
            if std::process::Command::new(&exe).arg("--updated").spawn().is_ok() {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            } else {
                APP.with(|a| a.borrow_mut().updating = false);
                balloon(hwnd, "Update installed", "Restart Marq to finish updating.", Balloon::None);
            }
        }
        InstallResult::Failed(e) => {
            APP.with(|a| a.borrow_mut().updating = false);
            balloon(hwnd, "Update failed", &e, Balloon::None);
        }
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = APP.with(|a| a.borrow().taskbar_created);
    if msg != 0 && msg == taskbar_created {
        let _ = add_tray_icon(hwnd); // Explorer restarted.
        return LRESULT(0);
    }
    match msg {
        WM_APP_TRAY => {
            match loword(lp.0 as usize) as u32 {
                NIN_SELECT | NIN_KEYSELECT => snip_soon(hwnd, Mode::Region, 150),
                WM_CONTEXTMENU => show_menu(hwnd, (loword(wp.0), hiword(wp.0))),
                NIN_BALLOONUSERCLICK => {
                    if APP.with(|a| a.borrow().balloon == Balloon::UpdateAvailable) {
                        start_install(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY | WM_APP_SNIP_KEY => {
            overlay::start(Mode::Region);
            LRESULT(0)
        }
        WM_APP_SNIP => {
            snip_soon(hwnd, Mode::Region, 50);
            LRESULT(0)
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_SNIP => {
                    let _ = KillTimer(Some(hwnd), TIMER_SNIP);
                    let mode = APP.with(|a| a.borrow().pending_snip);
                    overlay::start(mode);
                }
                TIMER_UPDATE_FIRST => {
                    let _ = KillTimer(Some(hwnd), TIMER_UPDATE_FIRST);
                    SetTimer(Some(hwnd), TIMER_UPDATE_PERIODIC, PERIODIC_CHECK_MS, None);
                    updater::check_async(hwnd, WM_APP_UPDATER, false);
                }
                TIMER_UPDATE_PERIODIC => updater::check_async(hwnd, WM_APP_UPDATER, false),
                TIMER_TRAY_RETRY => {
                    if add_tray_icon(hwnd) {
                        let _ = KillTimer(Some(hwnd), TIMER_TRAY_RETRY);
                    }
                }
                TIMER_TRIM => {
                    let _ = KillTimer(Some(hwnd), TIMER_TRIM);
                    trim_memory();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_UPDATER => {
            for ev in updater::take_events() {
                match ev {
                    Event::Checked { manual, result } => on_checked(hwnd, manual, result),
                    Event::Installed(result) => on_installed(hwnd, result),
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(Some(hwnd), HK_FALLBACK);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
