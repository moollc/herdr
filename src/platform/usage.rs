//! OS adapters used by account allowance readers.
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::Path,
};

// libc supplies the same POSIX local calendar on both supported platforms.
pub(crate) fn local_time(epoch: u64, format: &std::ffi::CStr) -> String {
    let Ok(time) = libc::time_t::try_from(epoch) else {
        return "?".into();
    };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut buf = [0u8; 80];
    // SAFETY: localtime_r initializes tm on success; strftime writes within buf.
    unsafe {
        if libc::localtime_r(&time, tm.as_mut_ptr()).is_null() {
            return "?".into();
        }
        let n = libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            format.as_ptr(),
            tm.as_ptr(),
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub(crate) fn lock_statusline(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    // SAFETY: a live descriptor is borrowed; closing File releases flock on all exits.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock)
}
#[cfg(target_os = "macos")]
pub(crate) fn cursor_cookie(service: &str) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cookie = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if !cookie.contains('=') || cookie.contains(['\r', '\n']) {
        return None;
    }
    Some(cookie)
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn cursor_cookie(_service: &str) -> Option<String> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorLoginResult {
    Exited(i32),
    #[cfg(any(target_os = "macos", test))]
    TimedOut,
    #[cfg(any(target_os = "macos", test))]
    Failed,
}

#[cfg(target_os = "macos")]
mod cursor_login;
#[cfg(target_os = "macos")]
pub(crate) use cursor_login::{cursor_login_cli, refresh_cursor_login};
#[cfg(not(target_os = "macos"))]
pub(crate) fn cursor_login_cli(_args: &[String]) -> i32 {
    eprintln!("Cursor browser login is available on macOS only (NoLogin).");
    3
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn refresh_cursor_login(_timeout: std::time::Duration) -> CursorLoginResult {
    CursorLoginResult::Exited(3)
}

pub(crate) fn speak_alert(command: &str, phrase: &str) {
    let mut parts = command.split_whitespace();
    let program = match parts.next() {
        Some(program) => program,
        None if cfg!(target_os = "macos") => "/usr/bin/say",
        None => return,
    };
    if let Ok(mut child) = std::process::Command::new(program)
        .args(parts)
        .arg(phrase)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        let _ = child.wait();
    }
}
