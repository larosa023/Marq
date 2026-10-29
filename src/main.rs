//! Marq — a hyper-lightweight screen snipping tool for Windows.
//! Lives in the system tray; snips go straight to the clipboard.

#![windows_subsystem = "windows"]

mod capture;
mod editor;
mod gfx;
mod overlay;
mod settings;
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
use updater::{CheckResult, InstallResult};
use util::*;

const HOST_CLASS: &str = "MarqHost";
const MUTEX_NAME: &str = "Local\\MarqSingleInstance-7f3a";

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SNIP: u32 = WM_APP + 2; // from a second instance; wParam = mode
const WM_APP_CHECKED: u32 = WM_APP + 3; // lParam = Box<CheckResult>
const WM_APP_INSTALLED: u32 = WM_APP + 4; // lParam = Box<InstallResult>

const HK_REGION: i32 = 1;
const HK_PRTSC: i32 = 2;

const TIMER_SNIP: usize = 1;
const TIMER_UPDATE_FIRST: usize = 2;
const TIMER_UPDATE_PERIODIC: usize = 3;
const FIRST_CHECK_MS: u32 = 10_000;
const PERIODIC_CHECK_MS: u32 = 6 * 60 * 60 * 1000;

const ID_REGION: usize = 100;
const ID_WINDOW: usize = 101;
const ID_SCREEN: usize = 102;
const ID_AUTOSTART: usize = 110;
const ID_UPDATE: usize = 111;
const ID_OPEN_RELEASES: usize = 112;
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
    copy_to_clipboard(host(), &img);
    toast::show(img, screen_rect);
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
        let wait_ms = if updated { 15_000 } else { 0 };
        let w = WaitForSingleObject(mutex, wait_ms);
        if w != WAIT_OBJECT_0 && w != WAIT_ABANDONED {
            let cls = WStr::new(HOST_CLASS);
            if let Ok(other) = FindWindowW(cls.p(), None) {
                let _ = PostMessageW(Some(other), WM_APP_SNIP, WPARAM(0), LPARAM(0));
            }
            return;
        }

        gfx::startup();
        overlay::register();
        toast::register();
        editor::register();
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

        add_tray_icon(hwnd);
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

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = ReleaseMutex(mutex);
    }
}

/// Ctrl+Alt+S always; Print Screen too when Windows isn't already using it.
fn register_hotkeys(hwnd: HWND) -> String {
    unsafe {
        let main_ok = RegisterHotKey(
            Some(hwnd),
            HK_REGION,
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
            VK_S.0 as u32,
        )
        .is_ok();
        let prtsc_ok = RegisterHotKey(Some(hwnd), HK_PRTSC, MOD_NOREPEAT, VK_SNAPSHOT.0 as u32).is_ok();
        match (main_ok, prtsc_ok) {
            (true, true) => "Ctrl+Alt+S or PrtScn".into(),
            (true, false) => "Ctrl+Alt+S".into(),
            (false, true) => "PrtScn".into(),
            (false, false) => "the tray icon".into(),
        }
    }
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

fn add_tray_icon(hwnd: HWND) {
    unsafe {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        let dpi = dpi_for_window(hwnd);
        nid.hIcon = app_icon(GetSystemMetricsForDpi(SM_CXSMICON, dpi));
        let hk = APP.with(|a| a.borrow().hotkey_label.clone());
        fill_wide(&mut nid.szTip, &format!("Marq \u{2014} {hk} to snip"));
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
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
        updater::install_async(hwnd, WM_APP_INSTALLED, version, sha);
    }
}

fn show_menu(hwnd: HWND) {
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
        let first_key = hk.split(" or ").next().unwrap_or("").to_string();

        add(0, &format!("Marq v{}", updater::CURRENT), MF_STRING | MF_GRAYED);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add(ID_REGION, &format!("Snip region\t{first_key}"), MF_STRING);
        add(ID_WINDOW, "Snip window", MF_STRING);
        add(ID_SCREEN, "Snip full screen", MF_STRING);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let auto = if settings::autostart_enabled() { MF_CHECKED } else { MF_UNCHECKED };
        add(ID_AUTOSTART, "Start with Windows", MF_STRING | auto);
        match (&update, updating) {
            (_, true) => add(ID_UPDATE, "Updating\u{2026}", MF_STRING | MF_GRAYED),
            (Some((v, _)), false) => add(ID_UPDATE, &format!("Update to v{v}"), MF_STRING),
            (None, false) => add(ID_UPDATE, "Check for updates", MF_STRING),
        }
        add(ID_OPEN_RELEASES, "What's new\u{2026}", MF_STRING);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add(ID_QUIT, "Quit", MF_STRING);
        if update.is_some() && !updating {
            let _ = SetMenuDefaultItem(menu, ID_UPDATE as u32, 0);
        }

        let (x, y) = cursor_pos();
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
            ID_AUTOSTART => settings::set_autostart(!settings::autostart_enabled()),
            ID_UPDATE => {
                if APP.with(|a| a.borrow().update.is_some()) {
                    start_install(hwnd);
                } else {
                    updater::check_async(hwnd, WM_APP_CHECKED, true);
                }
            }
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

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = APP.with(|a| a.borrow().taskbar_created);
    if msg != 0 && msg == taskbar_created {
        add_tray_icon(hwnd); // Explorer restarted.
        return LRESULT(0);
    }
    match msg {
        WM_APP_TRAY => {
            match loword(lp.0 as usize) as u32 {
                NIN_SELECT | NIN_KEYSELECT => snip_soon(hwnd, Mode::Region, 150),
                WM_CONTEXTMENU => show_menu(hwnd),
                NIN_BALLOONUSERCLICK => {
                    if APP.with(|a| a.borrow().balloon == Balloon::UpdateAvailable) {
                        start_install(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
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
                    updater::check_async(hwnd, WM_APP_CHECKED, false);
                }
                TIMER_UPDATE_PERIODIC => updater::check_async(hwnd, WM_APP_CHECKED, false),
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_CHECKED => {
            let res = *Box::from_raw(lp.0 as *mut CheckResult);
            let manual = wp.0 != 0;
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
            LRESULT(0)
        }
        WM_APP_INSTALLED => {
            let res = *Box::from_raw(lp.0 as *mut InstallResult);
            match res {
                InstallResult::Ready(exe) => {
                    // Hand over to the new version; it waits for this process to exit.
                    if std::process::Command::new(&exe).arg("--updated").spawn().is_ok() {
                        let _ = DestroyWindow(hwnd);
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
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(Some(hwnd), HK_REGION);
            let _ = UnregisterHotKey(Some(hwnd), HK_PRTSC);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
