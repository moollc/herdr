//! Linux Secret Service adapter and explicitly approved plaintext fallback.
use super::command::{execute, Output};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path},
    time::{Duration, Instant},
};
const ATTRS: &[&str] = &["application", "herdr", "purpose", "cursor-usage"];
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StoreError {
    Unavailable,
    Failed,
}
fn helper(args: &[&str], input: Option<&[u8]>) -> Result<Output, ()> {
    execute(
        "secret-tool",
        args,
        input,
        Instant::now() + Duration::from_secs(10),
    )
}
fn unavailable(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    (text.contains("org.freedesktop.secrets")
        && (text.contains("ServiceUnknown")
            || text.contains("not provided")
            || text.contains("not found")))
        || text.contains("Cannot autolaunch D-Bus")
        || text.contains("Cannot connect to session bus")
}
fn helper_installed() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|p| {
            std::fs::metadata(p.join("secret-tool"))
                .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        })
    })
}
fn store_with(
    secret: &str,
    installed: bool,
    run: impl FnOnce(&[&str], Option<&[u8]>) -> Result<Output, ()>,
) -> Result<(), StoreError> {
    if !installed {
        return Err(StoreError::Unavailable);
    }
    let mut args = vec!["store", "--label=Herdr Cursor usage"];
    args.extend(ATTRS);
    let output = run(&args, Some(secret.as_bytes())).map_err(|_| StoreError::Failed)?;
    if output.code == Some(0) {
        Ok(())
    } else if unavailable(&output.stderr) {
        Err(StoreError::Unavailable)
    } else {
        Err(StoreError::Failed)
    }
}
pub(super) fn store(secret: &str) -> Result<(), StoreError> {
    store_with(secret, helper_installed(), helper)
}

// Traverse directory descriptors so neither the credential directory nor any parent
// can redirect a write through a symlink. Existing ancestor permissions are untouched.
fn folder(root: &Path, create: bool) -> io::Result<OwnedFd> {
    use std::ffi::CString;
    if !root.is_absolute() {
        return Err(io::Error::other("state directory must be absolute"));
    }
    let mut directory: OwnedFd = File::open("/")?.into();
    for component in root.components() {
        let Component::Normal(name) = component else {
            if component == Component::RootDir {
                continue;
            }
            return Err(io::Error::other("invalid state directory"));
        };
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: directory is open and name is a live NUL-terminated component.
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            // SAFETY: same live descriptor and component; mkdir never follows final symlinks.
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if made != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: reopen with O_NOFOLLOW even if another process won creation.
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returns a uniquely owned descriptor.
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    let file: File = directory.into();
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "credential directory must be owned by you with mode 0700",
        ));
    }
    Ok(file.into())
}
fn credential(root: &Path, create: bool, writable: bool) -> io::Result<File> {
    let directory = folder(root, create)?;
    let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    flags |= if create {
        libc::O_RDWR | libc::O_CREAT
    } else if writable {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    };
    // SAFETY: directory is live; static filename is NUL terminated; mode is used only for creation.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), c"cookie".as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat transfers a new descriptor to File.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    // Reject hardlinks and special files before truncating or reading anything.
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "credential file must be private, regular and singly linked",
        ));
    }
    Ok(file)
}
fn root() -> std::path::PathBuf {
    crate::config::state_dir().join("usage/cursor-login")
}
fn file_read(root: &Path) -> io::Result<String> {
    let mut secret = String::new();
    let file = credential(root, false, false)?;
    // SAFETY: file is a live descriptor; the lock is released when it closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    file.take((super::manual_login::MAX_HEADER + 1) as u64)
        .read_to_string(&mut secret)?;
    if !super::manual_login::valid_header(&secret) {
        return Err(io::Error::other("invalid saved login"));
    }
    Ok(secret)
}
pub(super) fn plaintext(secret: &str) -> io::Result<()> {
    file_store(&root(), secret)
}
fn file_store(root: &Path, secret: &str) -> io::Result<()> {
    let mut file = credential(root, true, true)?;
    // SAFETY: file is a live descriptor; serialize truncation and complete writes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    file.set_len(0)?;
    file.write_all(secret.as_bytes())?;
    file.sync_all()
}
fn file_clear(root: &Path) -> io::Result<()> {
    match credential(root, false, true) {
        Ok(file) => {
            // SAFETY: file is a live descriptor; serialize clearing against reads and writes.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
            file.set_len(0)?;
            file.sync_all()
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub(super) fn clear_plaintext() -> io::Result<()> {
    file_clear(&root())
}
pub(super) fn read() -> Option<String> {
    if let Ok(secret) = file_read(&root()) {
        return Some(secret);
    }
    if let Ok(output) = helper(&[&["lookup"], ATTRS].concat(), None) {
        if output.code == Some(0) {
            let value = String::from_utf8(output.stdout).ok()?;
            // secret-tool adds one output newline, not part of the stored value.
            let value = value.strip_suffix('\n').unwrap_or(&value);
            return super::manual_login::valid_header(value).then(|| value.to_owned());
        }
    }
    None
}
fn forget_with(
    installed: bool,
    saved_in_keyring: bool,
    run: impl FnOnce(&[&str], Option<&[u8]>) -> Result<Output, ()>,
    clear_file: impl FnOnce() -> io::Result<()>,
) -> Result<(), ()> {
    if installed {
        let out = run(&[&["clear"], ATTRS].concat(), None)?;
        if unavailable(&out.stderr) && !saved_in_keyring {
            return clear_file().map_err(|_| ());
        }
        if !matches!(out.code, Some(0 | 1)) || !out.stderr.is_empty() {
            return Err(());
        }
    } else if saved_in_keyring {
        // Without the helper, an existing Secret Service item cannot be confirmed absent.
        return Err(());
    }
    clear_file().map_err(|_| ())
}
pub(super) fn forget() -> Result<(), ()> {
    let saved_in_keyring = crate::usage::read_json::<serde_json::Value>(
        &crate::config::state_dir().join("usage/cursor-login-status.json"),
    )
    .ok()
    .is_some_and(|status| {
        status.get("storage").and_then(|v| v.as_str()) == Some("secret-service")
            || status.get("keyring_may_exist").and_then(|v| v.as_bool()) == Some(true)
    });
    forget_with(
        helper_installed(),
        saved_in_keyring,
        helper,
        clear_plaintext,
    )
}
pub(super) fn storage_kind() -> &'static str {
    if file_read(&root()).is_ok() {
        "plaintext"
    } else {
        "secret-service"
    }
}
pub(super) fn storage_status() -> &'static str {
    if file_read(&root()).is_ok() {
        "plaintext file present"
    } else if helper_installed() {
        "Secret Service (presence checked on use)"
    } else {
        "Secret Service helper unavailable; no plaintext login"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> std::path::PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "herdr-usage-linux-{}-{}",
            std::process::id(),
            crate::usage::now()
        ));
        // Each test below uses distinct credential leaf directories.
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn usage_linux_private_file_rejects_symlinks_permissions_and_hardlinks() {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{symlink, PermissionsExt},
        };
        let base = directory();
        let root = base.join("private");
        file_store(&root, "token=synthetic").unwrap();
        assert_eq!(file_read(&root).unwrap(), "token=synthetic");
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(root.join("cookie")).unwrap().mode() & 0o777,
            0o600
        );
        let link = base.join("redirect");
        symlink(&root, &link).unwrap();
        assert!(file_store(&link, "token=wrong").is_err());
        let root2 = base.join("linkfile");
        std::fs::create_dir(&root2).unwrap();
        std::fs::set_permissions(&root2, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(root.join("cookie"), root2.join("cookie")).unwrap();
        assert!(file_store(&root2, "token=wrong").is_err());
        std::fs::hard_link(root.join("cookie"), base.join("hardlink")).unwrap();
        assert!(file_store(&root, "token=wrong").is_err());
        let root3 = base.join("clear");
        file_store(&root3, "token=synthetic").unwrap();
        let locked = credential(&root3, false, true).unwrap();
        // SAFETY: the synthetic credential descriptor is live and owned by this test.
        assert_eq!(unsafe { libc::flock(locked.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert!(file_read(&root3).is_err());
        assert!(file_store(&root3, "token=wrong").is_err());
        drop(locked);
        file_clear(&root3).unwrap();
        assert!(file_read(&root3).is_err());
        let root4 = base.join("public");
        std::fs::create_dir(&root4).unwrap();
        std::fs::set_permissions(&root4, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(file_store(&root4, "token=wrong").is_err());
        assert!(root.as_os_str().as_bytes().starts_with(b"/"));
    }
    #[test]
    fn usage_linux_forget_requires_confirmed_keyring_removal_and_only_herdr_attributes() {
        assert_eq!(
            forget_with(
                false,
                true,
                |_, _| panic!("missing helper"),
                || panic!("must retain consent")
            ),
            Err(())
        );
        assert_eq!(
            forget_with(false, false, |_, _| panic!("missing helper"), || Ok(())),
            Ok(())
        );
        for saved in [false, true] {
            let mut cleared = false;
            let result = forget_with(
                true,
                saved,
                |_, _| {
                    Ok(Output {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"org.freedesktop.secrets ServiceUnknown".to_vec(),
                    })
                },
                || {
                    cleared = true;
                    Ok(())
                },
            );
            assert_eq!(result, if saved { Err(()) } else { Ok(()) });
            assert_eq!(cleared, !saved);
        }
        for code in [0, 1, 2] {
            let mut cleared = false;
            let result = forget_with(
                true,
                true,
                |args, input| {
                    assert_eq!(
                        args,
                        ["clear", "application", "herdr", "purpose", "cursor-usage"]
                    );
                    assert!(input.is_none());
                    Ok(Output {
                        code: Some(code),
                        stdout: vec![],
                        stderr: vec![],
                    })
                },
                || {
                    cleared = true;
                    Ok(())
                },
            );
            assert_eq!(result, if code < 2 { Ok(()) } else { Err(()) });
            assert_eq!(cleared, code < 2);
        }
    }
    #[test]
    fn usage_linux_store_uses_stdin_and_distinguishes_locked_from_unavailable() {
        assert_eq!(
            store_with("token=synthetic", false, |_, _| panic!("missing helper")),
            Err(StoreError::Unavailable)
        );
        for (stderr, expected) in [
            (
                "org.freedesktop.secrets ServiceUnknown",
                Err(StoreError::Unavailable),
            ),
            ("locked collection", Err(StoreError::Failed)),
        ] {
            assert_eq!(
                store_with("token=synthetic", true, |args, input| {
                    assert!(!args.join(" ").contains("synthetic"));
                    assert!(args.windows(2).any(|a| a == ["application", "herdr"]));
                    assert_eq!(input, Some(b"token=synthetic".as_slice()));
                    Ok(Output {
                        code: Some(1),
                        stdout: vec![],
                        stderr: stderr.as_bytes().to_vec(),
                    })
                }),
                expected
            );
        }
        // Compile all Linux adapter entrypoints here, while never invoking real helpers.
        let _store = store;
        let _read = read;
        let _forget = forget;
        let _status = storage_status;
        let _kind = storage_kind;
        let _plain = plaintext;
    }
}
