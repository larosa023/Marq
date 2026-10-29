//! Self-update from GitHub Releases, driven from the tray menu.
//!
//! Each release publishes two assets:
//!   Marq.exe    – the app
//!   latest.txt  – line 1: version (e.g. 0.2.0), line 2: SHA-256 of Marq.exe
//!
//! Updating downloads Marq.exe, verifies its hash, swaps it in place of the running exe
//! (Windows allows renaming a running exe), restarts, and the new copy deletes the old one.

use std::ffi::OsString;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::*;
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::util::WStr;

pub const REPO_OWNER: &str = "larosa023";
pub const REPO_NAME: &str = "Marq";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub enum CheckResult {
    UpToDate,
    Available { version: String, sha256: String },
    Failed(String),
}

pub enum InstallResult {
    Ready(PathBuf),
    Failed(String),
}

/// Check in the background; posts `msg` to `hwnd` with a `Box<CheckResult>` in LPARAM.
pub fn check_async(hwnd: HWND, msg: u32, manual: bool) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || {
        let res = check();
        let boxed = Box::into_raw(Box::new(res));
        unsafe {
            if PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(manual as usize), LPARAM(boxed as isize)).is_err() {
                drop(Box::from_raw(boxed));
            }
        }
    });
}

/// Download + verify + swap in the background; posts `msg` with a `Box<InstallResult>`.
pub fn install_async(hwnd: HWND, msg: u32, version: String, sha256: String) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || {
        let res = install(&version, &sha256);
        let boxed = Box::into_raw(Box::new(res));
        unsafe {
            if PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                drop(Box::from_raw(boxed));
            }
        }
    });
}

fn asset_path(name: &str) -> String {
    format!("/{}/{}/releases/latest/download/{}", REPO_OWNER, REPO_NAME, name)
}

fn check() -> CheckResult {
    let body = match http_get("github.com", &asset_path("latest.txt")) {
        Ok(b) => b,
        Err(e) => return CheckResult::Failed(e),
    };
    let text = String::from_utf8_lossy(&body);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let version = lines.next().unwrap_or("").trim_start_matches('v').to_string();
    let sha256 = lines.next().unwrap_or("").to_ascii_lowercase();
    if parse_version(&version).is_none() || sha256.len() != 64 {
        return CheckResult::Failed("The release info on GitHub looks malformed.".into());
    }
    if is_newer(&version, CURRENT) {
        CheckResult::Available { version, sha256 }
    } else {
        CheckResult::UpToDate
    }
}

fn install(version: &str, expected_sha: &str) -> InstallResult {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return InstallResult::Failed(format!("Couldn't locate Marq.exe ({e}).")),
    };
    let bytes = match http_get("github.com", &asset_path("Marq.exe")) {
        Ok(b) => b,
        Err(e) => return InstallResult::Failed(e),
    };
    let actual = hex(&Sha256::digest(&bytes));
    if actual != expected_sha {
        return InstallResult::Failed(format!(
            "Downloaded v{version} failed its integrity check, so it wasn't installed."
        ));
    }

    let new_path = sibling(&exe, ".new");
    let old_path = sibling(&exe, ".old");
    if let Err(e) = std::fs::write(&new_path, &bytes) {
        let _ = std::fs::remove_file(&new_path);
        return InstallResult::Failed(permission_hint(e));
    }
    drop(bytes);
    let _ = retry(|| std::fs::remove_file(&old_path).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
    }));
    if let Err(e) = retry(|| std::fs::rename(&exe, &old_path)) {
        let _ = std::fs::remove_file(&new_path);
        return InstallResult::Failed(permission_hint(e));
    }
    if let Err(e) = retry(|| std::fs::rename(&new_path, &exe)) {
        // Roll back so the user is never left without an exe.
        let _ = retry(|| std::fs::rename(&old_path, &exe));
        let _ = std::fs::remove_file(&new_path);
        return InstallResult::Failed(permission_hint(e));
    }
    InstallResult::Ready(exe)
}

/// Antivirus often holds a freshly written exe open for a moment while scanning it.
/// Retry file operations for up to ~4 seconds before giving up.
fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut last = None;
    for _ in 0..20 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("retry failed")))
}

fn permission_hint(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        "Marq can't update itself in this folder. Move Marq.exe somewhere you own \
         (Documents, Desktop, or %LOCALAPPDATA%\\Marq) and try again."
            .into()
    } else {
        format!("Couldn't replace Marq.exe ({e}).")
    }
}

/// Delete the previous exe left behind by an update (it may take a moment to unlock),
/// plus any half-finished download.
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::fs::remove_file(sibling(&exe, ".new"));
    let old = sibling(&exe, ".old");
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

fn sibling(exe: &PathBuf, suffix: &str) -> PathBuf {
    let mut s: OsString = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").split('-').next()?.parse().ok()?;
    Some((a, b, c))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTPS GET via WinHTTP (built into Windows: no extra dependencies).
// Follows GitHub's redirect to its asset CDN automatically.
// ---------------------------------------------------------------------------

struct H(*mut core::ffi::c_void);
impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn http_get(host: &str, path: &str) -> Result<Vec<u8>, String> {
    let offline = || "Couldn't reach GitHub. Check your internet connection.".to_string();
    unsafe {
        let agent = WStr::new(&format!("Marq/{CURRENT}"));
        let mut session = H(WinHttpOpen(agent.p(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, None, None, 0));
        if session.0.is_null() {
            // Pre-8.1 fallback.
            session = H(WinHttpOpen(agent.p(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, None, None, 0));
        }
        if session.0.is_null() {
            return Err(offline());
        }
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 60_000);

        let host_w = WStr::new(host);
        let conn = H(WinHttpConnect(session.0, host_w.p(), INTERNET_DEFAULT_HTTPS_PORT, 0));
        if conn.0.is_null() {
            return Err(offline());
        }
        let verb = WStr::new("GET");
        let path_w = WStr::new(path);
        let req = H(WinHttpOpenRequest(
            conn.0,
            verb.p(),
            path_w.p(),
            None,
            None,
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(offline());
        }
        if WinHttpSendRequest(req.0, None, None, 0, 0, 0).is_err() {
            return Err(offline());
        }
        if WinHttpReceiveResponse(req.0, std::ptr::null_mut()).is_err() {
            return Err(offline());
        }

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let _ = WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            None,
            Some(&mut status as *mut u32 as *mut _),
            &mut len,
            std::ptr::null_mut(),
        );
        if status == 404 {
            return Err("No release has been published on GitHub yet.".into());
        }
        if status != 200 {
            return Err(format!("GitHub returned an error (HTTP {status})."));
        }

        let mut out = Vec::new();
        loop {
            let mut avail = 0u32;
            if WinHttpQueryDataAvailable(req.0, &mut avail).is_err() {
                return Err(offline());
            }
            if avail == 0 {
                break;
            }
            let start = out.len();
            out.resize(start + avail as usize, 0);
            let mut read = 0u32;
            if WinHttpReadData(req.0, out[start..].as_mut_ptr() as *mut _, avail, &mut read).is_err() {
                return Err(offline());
            }
            out.truncate(start + read as usize);
            if out.len() > 64 * 1024 * 1024 {
                return Err("Download was unexpectedly large.".into());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
    }
    #[test]
    fn sha() {
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
