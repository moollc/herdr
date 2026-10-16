//! Explicit paste-once login; terminal input and storage remain in the platform layer.
#[cfg(any(target_os = "linux", test))]
use super::consent::consent;
use super::{consent::save_consent, CursorLoginResult};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
pub(super) const MAX_HEADER: usize = 16 * 1024;
const HELP: &str = "usage: herdr usage cursor-login [--paste | --refresh | --status | --forget] [--browser <name>]\n\n--paste: Open your signed-in cursor.com page in your browser. In DevTools,\nselect Network, then a cursor.com request. Copy the Cookie header value\nand paste it at the hidden prompt. Do not paste a full cURL command.\nThe login is checked and saved. When it expires, paste again.\nAutomatic browser import is available on macOS; Linux import is deferred.";
fn status_file() -> PathBuf {
    crate::config::state_dir().join("usage/cursor-login-status.json")
}
fn pasted() -> bool {
    crate::usage::read_json::<serde_json::Value>(&status_file())
        .ok()
        .is_some_and(|s| s.get("kind").and_then(|s| s.as_str()) == Some("paste"))
}
pub(super) fn valid_header(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_HEADER
        || !value.is_ascii()
        || value.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return false;
    }
    value.split(';').all(|pair| {
        let Some((name, value)) = pair.trim().split_once('=') else {
            return false;
        };
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            && value.bytes().all(|b| {
                b == 0x21
                    || (0x23..=0x2b).contains(&b)
                    || (0x2d..=0x3a).contains(&b)
                    || (0x3c..=0x5b).contains(&b)
                    || (0x5d..=0x7e).contains(&b)
            })
    })
}
fn read_hidden(mut input: impl Read) -> io::Result<String> {
    let mut value = Vec::new();
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        match byte[0] {
            b'\n' | b'\r' => return String::from_utf8(value).map_err(io::Error::other),
            3 | 4 => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "paste cancelled",
                ));
            }
            8 | 127 => {
                value.pop();
            }
            byte if value.len() < MAX_HEADER => value.push(byte),
            _ => return Err(io::Error::other("Cookie header exceeds 16384 bytes")),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "paste cancelled",
    ))
}
struct TerminalMode {
    fd: libc::c_int,
    original: libc::termios,
}
impl TerminalMode {
    fn hidden(fd: libc::c_int) -> io::Result<Self> {
        let mut original = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes the termios buffer on success; stdin is checked as a TTY.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful tcgetattr initialized original.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        // SAFETY: hidden is initialized and stdin refers to the same terminal.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }
}
impl Drop for TerminalMode {
    fn drop(&mut self) {
        // SAFETY: restore the saved attributes, discarding pending oversized/cancelled input.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original);
        }
    }
}
fn prompt() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "--paste requires an interactive terminal; secrets cannot be passed as arguments or piped",
        ));
    }
    eprintln!("Copy only the Cookie header value from a cursor.com request in DevTools > Network.");
    let terminal = TerminalMode::hidden(libc::STDIN_FILENO)?;
    eprint!("Cookie header (hidden): ");
    io::stderr().flush()?;
    let result = read_hidden(io::stdin().lock());
    drop(terminal);
    eprintln!();
    result
}
#[cfg(any(target_os = "linux", test))]
fn confirm_plaintext() -> bool {
    eprint!(
        "Secret Service is unavailable. Save this login as plaintext in Herdr's private state directory (0600 file, 0700 folder)? [y/N] "
    );
    let _ = io::stderr().flush();
    let mut input = String::new();
    io::BufRead::read_line(&mut io::stdin().lock().take(32), &mut input).is_ok()
        && input.trim().eq_ignore_ascii_case("y")
}
fn finish_paste(
    secret: &str,
    check: impl FnOnce(&str) -> i32,
    save: impl FnOnce(&str) -> Result<bool, ()>,
    saved: impl FnOnce() -> std::io::Result<()>,
) -> i32 {
    if !valid_header(secret) {
        eprintln!(
            "Invalid Cookie header value. Copy only the value from a cursor.com request and paste again."
        );
        return 1;
    }
    let code = check(secret);
    if code != 0 {
        eprintln!(
            "Could not validate this login. If it expired, sign in to cursor.com and paste again."
        );
        return code;
    }
    match save(secret) {
        Ok(true) => {
            if saved().is_err() {
                eprintln!("Login saved, but could not schedule a fresh Cursor reading.");
                return 1;
            }
            println!(
                "Saved the cursor.com login. When it expires, run herdr usage cursor-login --paste again."
            );
            0
        }
        Ok(false) => {
            println!("Nothing was saved.");
            0
        }
        Err(()) => {
            eprintln!("Could not store the login.");
            1
        }
    }
}
#[cfg(target_os = "macos")]
fn save(secret: &str) -> Result<bool, ()> {
    let input = super::cursor_login::security_input(secret)?;
    let output = super::command::execute(
        "/usr/bin/security",
        &["-i"],
        Some(input.as_bytes()),
        Instant::now() + Duration::from_secs(10),
    )?;
    if output.code != Some(0) {
        return Err(());
    }
    Ok(true)
}
#[cfg(target_os = "linux")]
fn save(secret: &str) -> Result<bool, ()> {
    save_linux(
        secret,
        |secret| {
            super::linux_login::store(secret)?;
            super::linux_login::clear_plaintext()
                .map_err(|_| super::linux_login::StoreError::Failed)
        },
        confirm_plaintext,
        super::linux_login::plaintext,
    )
}
#[cfg(any(target_os = "linux", test))]
fn save_linux(
    secret: &str,
    store: impl FnOnce(&str) -> Result<(), super::linux_login::StoreError>,
    ask: impl FnOnce() -> bool,
    plaintext: impl FnOnce(&str) -> io::Result<()>,
) -> Result<bool, ()> {
    match store(secret) {
        Ok(()) => Ok(true),
        Err(super::linux_login::StoreError::Unavailable) if ask() => {
            plaintext(secret).map(|()| true).map_err(|_| ())
        }
        Err(super::linux_login::StoreError::Unavailable) => Ok(false),
        Err(super::linux_login::StoreError::Failed) => Err(()),
    }
}
fn storage_kind() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "keychain"
    }
    #[cfg(target_os = "linux")]
    {
        super::linux_login::storage_kind()
    }
}
fn paste() -> i32 {
    let secret = match prompt() {
        Ok(secret) => secret,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    finish_paste(
        &secret,
        |secret| super::validation::check_usage(secret, Instant::now() + Duration::from_secs(5)),
        |secret| {
            let previous =
                crate::usage::read_json::<serde_json::Value>(&status_file()).unwrap_or_default();
            let previous_keyring = previous.get("storage").and_then(|v| v.as_str())
                == Some("secret-service")
                || previous.get("keyring_may_exist").and_then(|v| v.as_bool()) == Some(true);
            if !save(secret)? {
                return Ok(false);
            }
            save_consent(&crate::config::config_path(), true).map_err(|_| ())?;
            crate::usage::atomic_json(&status_file(), &serde_json::json!({"kind":"paste", "browser":"pasted", "reason":"explicit --paste", "result":"saved", "storage":storage_kind(), "keyring_may_exist": previous_keyring || storage_kind() == "secret-service"})).map_err(|_| ())?;
            Ok(true)
        },
        || crate::usage::cursor_login_saved(&crate::usage::UsageConfig::configured()),
    )
}
pub(crate) fn cursor_login_cli(args: &[String]) -> i32 {
    if args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return 0;
    }
    if args.iter().any(|a| a == "--paste") {
        if args != ["--paste"] {
            eprintln!("--paste must be used alone.\n{HELP}");
            return 1;
        }
        return paste();
    }
    if super::arguments::parse_args(args).is_err() {
        eprintln!("{HELP}");
        return 1;
    }
    if args.iter().any(|a| a == "--refresh") && pasted() {
        eprintln!(
            "A pasted login cannot refresh itself. Run herdr usage cursor-login --paste again."
        );
        return 4;
    }
    #[cfg(target_os = "macos")]
    {
        super::cursor_login::cursor_login_cli(args)
    }
    #[cfg(target_os = "linux")]
    {
        linux_cli(args)
    }
}
#[cfg(any(target_os = "linux", test))]
fn linux_cli(args: &[String]) -> i32 {
    if args == ["--status"] {
        let status =
            crate::usage::read_json::<serde_json::Value>(&status_file()).unwrap_or_default();
        println!(
            "consent: {}\nstorage: {}\nlast: {}\nsource: {}",
            if consent() { "yes" } else { "no" },
            super::linux_login::storage_status(),
            status
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("none"),
            status
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
        );
        return 0;
    }
    if args == ["--forget"] {
        if super::linux_login::forget().is_err() {
            eprintln!("Could not confirm removal of the Herdr login; consent was not cleared.");
            return 1;
        }
        if save_consent(&crate::config::config_path(), false).is_err()
            || crate::usage::atomic_json(&status_file(), &serde_json::json!({"result":"forgotten"}))
                .is_err()
        {
            eprintln!("Could not clear the Herdr login settings.");
            return 1;
        }
        println!("Removed the Herdr login and consent.");
        return 0;
    }
    if !args.is_empty() && args != ["--refresh"] {
        eprintln!("{HELP}");
        return 1;
    }
    eprintln!(
        "{}",
        super::browser::default_browser()
            .unsupported
            .unwrap_or_else(|| {
                "Linux automatic cookie import is deferred; use herdr usage cursor-login --paste."
                    .into()
            })
    );
    3
}
pub(crate) fn refresh_cursor_login(timeout: Duration) -> CursorLoginResult {
    if pasted() {
        return CursorLoginResult::Exited(4);
    }
    #[cfg(target_os = "macos")]
    {
        super::cursor_login::refresh_cursor_login(timeout)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = timeout;
        CursorLoginResult::Exited(3)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_hidden_terminal_disables_echo_and_restores_on_cancel() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes two descriptors; optional settings/name pointers are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: successful openpty transfers ownership of both new descriptors.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: slave is the other uniquely owned descriptor returned by openpty.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let terminal = TerminalMode::hidden(slave.as_raw_fd()).unwrap();
        let mut attrs = std::mem::MaybeUninit::uninit();
        // SAFETY: attrs is a live output buffer and slave is an open terminal.
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr succeeded above.
        assert_eq!(
            unsafe { attrs.assume_init() }.c_lflag & (libc::ECHO | libc::ECHONL | libc::ISIG),
            0
        );
        let original_flags = terminal.original.c_lflag;
        master.write_all(b"synthetic-secret\x03").unwrap();
        assert!(read_hidden(&slave).is_err());
        drop(terminal);
        let mut restored = std::mem::MaybeUninit::uninit();
        // SAFETY: restored is live output storage and the descriptor remains open.
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), restored.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr succeeded above.
        assert_eq!(unsafe { restored.assume_init() }.c_lflag, original_flags);
        // A nonblocking master read must not reveal the synthetic secret as echo.
        // SAFETY: master is a live file descriptor; F_SETFL accepts these status flags.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        let mut echo = [0; 64];
        assert!(master
            .read(&mut echo)
            .is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock));
    }
    #[test]
    fn usage_paste_rejects_headers_commands_controls_and_oversize_before_validation() {
        for secret in [
            "",
            "Cookie: token=x",
            "curl https://cursor.com",
            "x=y\nz=z",
            "x=y\0",
            "x=y;",
            "bad name=x",
        ] {
            assert_eq!(
                finish_paste(
                    secret,
                    |_| panic!("validation"),
                    |_| panic!("storage"),
                    || Ok(())
                ),
                1
            );
        }
        assert!(!valid_header(&"x".repeat(MAX_HEADER + 1)));
        assert!(valid_header("token=synthetic%3D; __Secure-session=abc=="));
        assert_eq!(read_hidden(&b"x=ab\x7fc\n"[..]).unwrap(), "x=ac");
        assert!(read_hidden(&b"x=secret\x03"[..]).is_err());
        assert!(read_hidden(&vec![b'x'; MAX_HEADER + 1][..]).is_err());
        assert_eq!(
            finish_paste("x=synthetic", |_| 4, |_| panic!("store expired"), || Ok(())),
            4
        );
        assert_eq!(
            finish_paste("x=synthetic", |_| 0, |_| Ok(false), || Ok(())),
            0
        );
        assert_eq!(
            finish_paste("x=synthetic", |_| 0, |_| Err(()), || Ok(())),
            1
        );
        assert_eq!(
            finish_paste("x=synthetic", |_| 0, |_| Ok(true), || Ok(())),
            0
        );
    }
    #[test]
    fn usage_plaintext_requires_one_explicit_confirmation_and_denial_keeps_nothing() {
        use super::super::linux_login::StoreError;
        assert_eq!(
            save_linux(
                "x=synthetic",
                |_| Err(StoreError::Unavailable),
                || false,
                |_| panic!("declined")
            ),
            Ok(false)
        );
        let mut asks = 0;
        assert_eq!(
            save_linux(
                "x=synthetic",
                |_| Err(StoreError::Unavailable),
                || {
                    asks += 1;
                    true
                },
                |_| Ok(())
            ),
            Ok(true)
        );
        assert_eq!(asks, 1);
        assert_eq!(
            save_linux(
                "x=synthetic",
                |_| Err(StoreError::Failed),
                || panic!("locked keyring must not downgrade"),
                |_| panic!("plaintext")
            ),
            Err(())
        );
        assert_eq!(
            save_linux(
                "x=synthetic",
                |_| Ok(()),
                || panic!("no prompt"),
                |_| panic!("plaintext")
            ),
            Ok(true)
        );
        let _linux_cli: fn(&[String]) -> i32 = linux_cli;
        let _prompt: fn() -> bool = confirm_plaintext;
        let _consent: fn() -> bool = consent;
    }
    #[test]
    fn only_saved_login_requests_a_fresh_reading() {
        let config = crate::usage::UsageConfig {
            snapshot_file: std::env::temp_dir()
                .join(format!(
                    "herdr-saved-login-{}-{}-{}",
                    module_path!(),
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ))
                .join("usage.json"),
            ..crate::usage::UsageConfig::default()
        };
        let timer = config.snapshot_file.with_extension("cursor-fetch");
        crate::usage::atomic_json(&timer, &1000_u64).expect("timer");
        let requested = std::cell::Cell::new(0);
        let saved = || {
            requested.set(requested.get() + 1);
            crate::usage::cursor_login_saved(&config)
        };
        assert_eq!(
            finish_paste(
                "token=synthetic",
                |_| 4,
                |_| panic!("validation failed"),
                saved
            ),
            4
        );
        assert_eq!(
            finish_paste("token=synthetic", |_| 0, |_| Ok(false), saved),
            0
        );
        assert_eq!(requested.get(), 0);
        assert_eq!(
            crate::usage::read_json::<u64>(&timer).expect("unchanged timer"),
            1000
        );
        assert!(
            !config.snapshot_file.exists(),
            "failed or declined login leaves cache alone"
        );
        assert_eq!(
            finish_paste("token=synthetic", |_| 0, |_| Ok(true), saved),
            0
        );
        assert_eq!(requested.get(), 1);
        assert_eq!(
            crate::usage::read_json::<Option<u64>>(&timer).expect("reset timer"),
            None
        );
    }
}
