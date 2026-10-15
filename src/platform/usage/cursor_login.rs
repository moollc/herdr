//! Scoped macOS browser import. Secrets stay in memory and enter security over stdin.
use super::CursorLoginResult;
use std::{
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const SERVICE: &str = "herdr-cursor-usage";
const SIGNED_OUT: &str =
    "Your browser is not signed in to cursor.com. Sign in there, then run this again.";
const SQL: &str = "SELECT hex(host_key), hex(name), hex(encrypted_value), expires_utc, has_expires FROM cookies WHERE host_key IN ('cursor.com','.cursor.com');";

#[link(name = "System")]
unsafe extern "C" {
    fn CCKeyDerivationPBKDF(
        algorithm: u32,
        password: *const u8,
        password_len: usize,
        salt: *const u8,
        salt_len: usize,
        prf: u32,
        rounds: u32,
        key: *mut u8,
        key_len: usize,
    ) -> i32;
    fn CCCrypt(
        op: u32,
        algorithm: u32,
        options: u32,
        key: *const u8,
        key_len: usize,
        iv: *const u8,
        input: *const u8,
        input_len: usize,
        output: *mut u8,
        output_len: usize,
        moved: *mut usize,
    ) -> i32;
    fn CC_SHA256(data: *const u8, len: u32, digest: *mut u8) -> *mut u8;
}
fn derive(password: &[u8]) -> Result<[u8; 16], ()> {
    let mut key = [0; 16];
    // SAFETY: all buffers are live and sized by their accompanying lengths.
    let status = unsafe {
        CCKeyDerivationPBKDF(
            2,
            password.as_ptr(),
            password.len(),
            b"saltysalt".as_ptr(),
            9,
            1,
            1003,
            key.as_mut_ptr(),
            key.len(),
        )
    };
    if status == 0 {
        Ok(key)
    } else {
        Err(())
    }
}
fn crypt(op: u32, input: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, ()> {
    let mut output = vec![0; input.len() + 16];
    let mut moved = 0;
    // SAFETY: AES key and IV are 16 bytes; output includes one padding block.
    let status = unsafe {
        CCCrypt(
            op,
            0,
            1,
            key.as_ptr(),
            key.len(),
            [b' '; 16].as_ptr(),
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut moved,
        )
    };
    if status != 0 {
        return Err(());
    }
    output.truncate(moved);
    Ok(output)
}
fn host_hash(host: &str) -> [u8; 32] {
    let mut hash = [0; 32];
    // SAFETY: host is a fixed scoped hostname and hash holds SHA256's 32 bytes.
    unsafe {
        CC_SHA256(host.as_ptr(), host.len() as u32, hash.as_mut_ptr());
    }
    hash
}
fn decrypt(blob: &[u8], key: &[u8; 16], host: &str, version: u32) -> Result<String, ()> {
    if !blob.starts_with(b"v10") && !blob.starts_with(b"v11") {
        return Err(());
    }
    let plain = crypt(1, &blob[3..], key)?;
    let plain = if version >= 24 {
        if plain.len() <= 32 || plain[..32] != host_hash(host) {
            return Err(());
        }
        &plain[32..]
    } else {
        &plain
    };
    String::from_utf8(plain.to_vec()).map_err(|_| ())
}
fn unhex(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    s.as_bytes()
        .chunks(2)
        .map(|c| {
            let hi = (c[0] as char).to_digit(16).ok_or(())?;
            let lo = (c[1] as char).to_digit(16).ok_or(())?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}
fn uri(path: &Path, immutable: bool) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file:");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str(if immutable {
        "?mode=ro&immutable=1"
    } else {
        "?mode=ro"
    });
    uri
}
struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
/// Every helper shares an overall deadline. Drain pipes concurrently to avoid blocking
/// on a large database response; a timeout kills and reaps the helper before returning.
fn execute(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    deadline: Instant,
) -> Result<Output, ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ())?;
    let stdout = child.stdout.take().ok_or(())?;
    let stderr = child.stderr.take().ok_or(())?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.by_ref().take(8 * 1024 * 1024).read_to_end(&mut buf);
            buf
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let writer = input.map(|input| {
        let input = input.to_vec();
        let stdin = child.stdin.take();
        std::thread::spawn(move || {
            stdin
                .ok_or(())
                .and_then(|mut stdin| stdin.write_all(&input).map_err(|_| ()))
        })
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = out.join().map_err(|_| ())?;
    let stderr = err.join().map_err(|_| ())?;
    if let Some(writer) = writer {
        writer.join().map_err(|_| ())??;
    }
    let status = status.ok_or(())?;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
    })
}
fn database(path: &Path, deadline: Instant) -> Result<(u32, String), ()> {
    database_with(path, |uri, sql| {
        execute(
            "/usr/bin/sqlite3",
            &["-readonly", "-separator", "\t", uri, sql],
            None,
            deadline,
        )
    })
}
fn database_with(
    path: &Path,
    mut run: impl FnMut(&str, &str) -> Result<Output, ()>,
) -> Result<(u32, String), ()> {
    for immutable in [false, true] {
        let uri = uri(path, immutable);
        let result = (|| {
            let meta = run(&uri, "SELECT value FROM meta WHERE key = 'version';")?;
            if meta.code != Some(0) {
                return Ok(Err(meta));
            }
            let rows = run(&uri, SQL)?;
            if rows.code != Some(0) {
                return Ok(Err(rows));
            }
            Ok(Ok((
                String::from_utf8_lossy(&meta.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0),
                String::from_utf8(rows.stdout).map_err(|_| ())?,
            )))
        })()?;
        match result {
            Ok(data) => return Ok(data),
            Err(output)
                if String::from_utf8_lossy(&output.stderr)
                    .contains("unable to open database file") => {}
            Err(_) => return Err(()),
        }
    }
    Err(())
}
fn header(
    rows: &str,
    version: u32,
    now: u64,
    mut decrypt: impl FnMut(&[u8], &str, u32) -> Result<String, ()>,
) -> Result<String, ()> {
    let mut kept = Vec::new();
    for row in rows.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        if fields.len() != 5 {
            return Err(());
        }
        let host = String::from_utf8(unhex(fields[0])?).map_err(|_| ())?;
        if host != "cursor.com" && host != ".cursor.com" {
            continue;
        }
        if fields[4] == "1" {
            let expiry = fields[3].parse::<u64>().unwrap_or(0) / 1_000_000;
            if expiry.saturating_sub(11_644_473_600) <= now {
                continue;
            }
        }
        let name = String::from_utf8(unhex(fields[1])?).map_err(|_| ())?;
        if name.is_empty() || name.contains(['\r', '\n', '\0', ';', '=']) {
            continue;
        }
        let blob = unhex(fields[2])?;
        let Ok(value) = decrypt(&blob, &host, version) else {
            continue;
        };
        if value.is_empty() || value.contains(['\r', '\n', '\0', ';']) {
            continue;
        }
        kept.push((name, host, value));
    }
    kept.sort();
    Ok(kept
        .into_iter()
        .map(|(name, _, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; "))
}
#[derive(Clone)]
struct Browser {
    name: &'static str,
    path: PathBuf,
}
fn browsers(home: &Path) -> Vec<Browser> {
    let mut found = Vec::new();
    for (name, dir) in [
        ("Chrome", "Google/Chrome"),
        ("Brave", "BraveSoftware/Brave-Browser"),
    ] {
        let root = home.join("Library/Application Support").join(dir);
        let mut profiles = vec![root.join("Default")];
        let mut other = fs::read_dir(&root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("Profile "))
            })
            .collect::<Vec<_>>();
        other.sort();
        profiles.extend(other);
        for profile in profiles {
            for path in [profile.join("Cookies"), profile.join("Network/Cookies")] {
                if path.is_file() {
                    found.push(Browser { name, path });
                }
            }
        }
    }
    found
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Presence {
    Present,
    Absent,
    Unknown,
}
fn presence(code: Option<i32>) -> Presence {
    match code {
        Some(0) => Presence::Present,
        Some(44) => Presence::Absent,
        _ => Presence::Unknown,
    }
}
fn keychain_presence(deadline: Instant) -> Presence {
    presence(
        execute(
            "/usr/bin/security",
            &["find-generic-password", "-s", SERVICE],
            None,
            deadline,
        )
        .ok()
        .and_then(|o| o.code),
    )
}
fn security_input(secret: &str) -> Result<String, ()> {
    if secret.is_empty() || secret.contains(['\r', '\n', '\0']) {
        return Err(());
    }
    let escaped = secret.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "add-generic-password -U -s \"{SERVICE}\" -a \"herdr\" -w \"{escaped}\"\n"
    ))
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct LoginStatus {
    browser: String,
    result: String,
}
fn status_file() -> PathBuf {
    crate::config::state_dir().join("usage/cursor-login-status.json")
}
fn record(browser: &str, code: i32) {
    let status = LoginStatus {
        browser: browser.into(),
        result: match code {
            0 => "saved",
            4 => "signed_out",
            _ => "error",
        }
        .into(),
    };
    let _ = crate::usage::atomic_json(&status_file(), &status);
}
fn consent() -> bool {
    crate::config::Config::load().config.ui.usage.cursor_login
}
fn save_consent(path: &Path, enabled: bool) -> io::Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    // Refuse to overwrite a broken config with defaults.
    toml::from_str::<crate::config::Config>(&content).map_err(io::Error::other)?;
    let content = crate::config::upsert_section_bool(&content, "ui.usage", "cursor_login", enabled);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}
fn forget(
    clear: impl FnOnce() -> io::Result<()>,
    delete: Option<i32>,
    lookup: impl FnOnce() -> Presence,
) -> i32 {
    let confirmed = delete == Some(0) || delete == Some(44) || lookup() == Presence::Absent;
    if !confirmed {
        eprintln!("Could not confirm the Keychain item was removed. Consent was not cleared.");
        return 1;
    }
    if clear().is_err() {
        eprintln!("Could not clear consent in the Herdr config.");
        return 1;
    }
    if delete == Some(0) || delete == Some(44) {
        println!("Removed consent and the Keychain item.");
        0
    } else {
        eprintln!("The Keychain delete failed. The item is already absent. Consent was cleared.");
        1
    }
}
fn check_usage(secret: &str, deadline: Instant) -> i32 {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return 1;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(remaining.min(Duration::from_secs(5))))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let Ok(mut response) = agent
        .get("https://cursor.com/api/usage-summary")
        .header("Cookie", secret)
        .call()
    else {
        return 1;
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if status != 200 {
        return validation(status, location, None);
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_json::<serde_json::Value>()
        .ok();
    validation(status, "", body.as_ref())
}
fn validation(status: u16, location: &str, body: Option<&serde_json::Value>) -> i32 {
    if status == 401
        || ((300..400).contains(&status) && location.starts_with("https://api.workos.com/"))
    {
        return 4;
    }
    if status == 200
        && body
            .and_then(|v| v.get("billingCycleEnd"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
    {
        0
    } else {
        1
    }
}

fn finish_import(
    import: impl FnOnce() -> Result<String, ()>,
    check: impl FnOnce(&str) -> i32,
    save: impl FnOnce(&str) -> Result<(), ()>,
) -> i32 {
    let Ok(secret) = import() else {
        eprintln!("Could not read cursor.com cookies from the browser.");
        return 1;
    };
    if secret.is_empty() {
        eprintln!("{SIGNED_OUT}");
        return 4;
    }
    let code = check(&secret);
    if code != 0 {
        eprintln!("Could not validate the cursor.com login. Sign in there and try again.");
        return code;
    }
    if save(&secret).is_err() {
        eprintln!("Could not store the login in the Keychain.");
        return 1;
    }
    println!("Saved the cursor.com login to the Keychain.");
    0
}
fn import(browser: &Browser, deadline: Instant) -> i32 {
    let code = finish_import(
        || {
            let (version, rows) = database(&browser.path, deadline)?;
            let service = format!("{} Safe Storage", browser.name);
            let output = execute(
                "/usr/bin/security",
                &["find-generic-password", "-w", "-s", &service],
                None,
                deadline,
            )?;
            if output.code != Some(0) {
                return Err(());
            }
            let mut password = output.stdout;
            if password.last() == Some(&b'\n') {
                password.pop();
            }
            if password.is_empty() {
                return Err(());
            }
            let key = derive(&password)?;
            header(
                &rows,
                version,
                crate::usage::now(),
                |blob, host, version| decrypt(blob, &key, host, version),
            )
        },
        |secret| check_usage(secret, deadline),
        |secret| {
            let input = security_input(secret)?;
            let output = execute(
                "/usr/bin/security",
                &["-i"],
                Some(input.as_bytes()),
                deadline,
            )?;
            if output.code == Some(0) {
                Ok(())
            } else {
                Err(())
            }
        },
    );
    record(browser.name, code);
    code
}
fn authorized(
    refresh: bool,
    saved: bool,
    answer: impl FnOnce() -> bool,
    save: impl FnOnce() -> io::Result<()>,
) -> Result<bool, i32> {
    if saved {
        return Ok(true);
    }
    if refresh {
        return Err(3);
    }
    if !answer() {
        return Ok(false);
    }
    save().map_err(|_| 1)?;
    Ok(true)
}
pub(crate) fn cursor_login_cli(args: &[String]) -> i32 {
    let flag = args.first().map(String::as_str).unwrap_or("");
    if args.len() > 1 || !["", "--refresh", "--status", "--forget"].contains(&flag) {
        eprintln!("usage: herdr usage cursor-login [--refresh | --status | --forget]");
        return 1;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    if flag == "--status" {
        let status: LoginStatus = crate::usage::read_json(&status_file()).unwrap_or_default();
        println!(
            "browser: {}\nlast: {}",
            if status.browser.is_empty() {
                "none"
            } else {
                &status.browser
            },
            if status.result.is_empty() {
                "none"
            } else {
                &status.result
            }
        );
        println!(
            "consent: {}\nkeychain: {}",
            if consent() { "yes" } else { "no" },
            match keychain_presence(deadline) {
                Presence::Present => "present",
                Presence::Absent => "absent",
                Presence::Unknown => "unknown",
            }
        );
        return 0;
    }
    if flag == "--forget" {
        let delete = execute(
            "/usr/bin/security",
            &["delete-generic-password", "-s", SERVICE],
            None,
            deadline,
        )
        .ok()
        .and_then(|o| o.code);
        return forget(
            || {
                save_consent(&crate::config::config_path(), false)?;
                crate::usage::atomic_json(
                    &status_file(),
                    &LoginStatus {
                        browser: String::new(),
                        result: "forgotten".into(),
                    },
                )
            },
            delete,
            || keychain_presence(deadline),
        );
    }
    let saved = consent();
    if flag == "--refresh" && !saved {
        eprintln!("No consent saved. Run herdr usage cursor-login first.");
        return 3;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let Some(browser) = browsers(&home).into_iter().next() else {
        eprintln!("No Chrome or Brave cookie store found.");
        return 1;
    };
    let granted = authorized(
        flag == "--refresh",
        saved,
        || {
            println!(
                "This reads cursor.com cookies from your {} browser on this Mac.",
                browser.name
            );
            println!("It stores that login in the macOS Keychain item {SERVICE}.");
            print!(
                "Use your {} cursor.com login for the Herdr usage bars? [y/N] ",
                browser.name
            );
            let _ = io::stdout().flush();
            let mut answer = String::new();
            io::stdin().lock().read_line(&mut answer).is_ok()
                && answer.trim().eq_ignore_ascii_case("y")
        },
        || save_consent(&crate::config::config_path(), true),
    );
    match granted {
        Ok(true) => import(&browser, Instant::now() + Duration::from_secs(20)),
        Ok(false) => {
            println!("Nothing was saved.");
            0
        }
        Err(code) => code,
    }
}
pub(crate) fn refresh_cursor_login(timeout: Duration) -> CursorLoginResult {
    if !consent() {
        return CursorLoginResult::Exited(3);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let Some(browser) = browsers(&home).into_iter().next() else {
        return CursorLoginResult::Exited(1);
    };
    let deadline = Instant::now() + timeout;
    let code = import(&browser, deadline);
    if Instant::now() >= deadline {
        CursorLoginResult::TimedOut
    } else if code == 1 {
        CursorLoginResult::Failed
    } else {
        CursorLoginResult::Exited(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-cookie-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }
    fn encrypted(value: &str, host: &str, version: u32, key: &[u8; 16]) -> Vec<u8> {
        let mut plain = if version >= 24 {
            host_hash(host).to_vec()
        } else {
            Vec::new()
        };
        plain.extend(value.as_bytes());
        let mut blob = b"v10".to_vec();
        blob.extend(crypt(0, &plain, key).expect("encrypt synthetic vector"));
        blob
    }
    fn row(host: &str, name: &str, blob: &[u8], expiry: u64, expires: bool) -> String {
        format!(
            "{}\t{}\t{}\t{expiry}\t{}\n",
            hex(host.as_bytes()),
            hex(name.as_bytes()),
            hex(blob),
            u8::from(expires)
        )
    }
    #[test]
    fn usage_http_validation_without_requests() {
        for status in [301, 302, 303, 307, 308, 401] {
            assert_eq!(validation(status, "https://api.workos.com/login", None), 4);
        }
        for status in [302, 403, 404, 429, 500] {
            assert_eq!(validation(status, "https://other.example/", None), 1);
        }
        assert_eq!(validation(200, "", None), 1);
        for body in [
            serde_json::json!({}),
            serde_json::json!({"billingCycleEnd":""}),
        ] {
            assert_eq!(validation(200, "", Some(&body)), 1);
        }
        assert_eq!(
            validation(
                200,
                "",
                Some(&serde_json::json!({"billingCycleEnd":"2026-01-01T00:00:00Z"}))
            ),
            0
        );
    }
    #[test]
    fn usage_crypto_vectors_both_plaintext_layouts_and_host_binding() {
        let key = derive(b"synthetic-password").unwrap();
        for version in [23, 24, 30] {
            let blob = encrypted("synthetic-value", "cursor.com", version, &key);
            assert_eq!(
                decrypt(&blob, &key, "cursor.com", version).unwrap(),
                "synthetic-value"
            );
            if version >= 24 {
                assert!(decrypt(&blob, &key, ".cursor.com", version).is_err());
            }
            assert!(decrypt(&blob, &derive(b"wrong").unwrap(), "cursor.com", version).is_err());
        }
        assert!(decrypt(b"v20invalid", &key, "cursor.com", 24).is_err());
    }
    #[test]
    fn usage_synthetic_database_scope_expiry_and_no_copy() {
        let root = directory();
        let path = root.join("synthetic cookies #.sqlite");
        let key = derive(b"synthetic-password").unwrap();
        for version in [23, 24] {
            let path = root.join(format!("synthetic-{version}.sqlite"));
            let valid = encrypted("synthetic-value", "cursor.com", version, &key);
            let sql=format!("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','{version}'); CREATE TABLE cookies(host_key TEXT,name TEXT,encrypted_value BLOB,expires_utc INTEGER,has_expires INTEGER); INSERT INTO cookies VALUES('cursor.com','token',X'{}',0,0),('.cursor.com','expired',X'{}',11644473601000000,1),('other.example','private',X'00',0,0);",hex(&valid),hex(&valid));
            assert!(Command::new("/usr/bin/sqlite3")
                .arg(&path)
                .arg(sql)
                .status()
                .unwrap()
                .success());
            let before = fs::read(&path).unwrap();
            let (v, rows) = database(&path, Instant::now() + Duration::from_secs(2)).unwrap();
            assert!(!rows.contains(&hex(b"other.example")));
            let mut calls = 0;
            assert_eq!(
                header(&rows, v, 100, |blob, host, version| {
                    calls += 1;
                    decrypt(blob, &key, host, version)
                })
                .unwrap(),
                "token=synthetic-value"
            );
            assert_eq!(calls, 1);
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(
                fs::read_dir(&root).unwrap().count(),
                if version == 23 { 1 } else { 2 }
            );
        }
        assert!(uri(&path, false).contains("%20"));
        assert!(uri(&path, false).contains("%23"));
    }
    #[test]
    fn usage_other_hosts_never_decrypted_and_invalid_headers_rejected() {
        let mut rows = row("other.example", "token", b"bad", 0, false);
        rows += &row("cursor.com", "expired", b"bad", 11644473601000000, true);
        rows += &row("cursor.com", "bad=name", b"bad", 0, false);
        assert_eq!(
            header(&rows, 24, 100, |_, _, _| panic!("unscoped decrypt")).unwrap(),
            ""
        );
        let rows = row("cursor.com", "token", b"v10fake", 0, false);
        for value in ["", "x;y", "x\ny", "x\0y"] {
            assert_eq!(
                header(&rows, 23, 100, |_, _, _| Ok(value.into())).unwrap(),
                ""
            );
        }
    }
    #[test]
    fn usage_readonly_fallback_only_on_open_error() {
        let mut calls = Vec::new();
        let result = database_with(Path::new("/synthetic/cookies"), |uri, sql| {
            calls.push((uri.to_owned(), sql.to_owned()));
            if !uri.contains("immutable") {
                Ok(Output {
                    code: Some(1),
                    stdout: vec![],
                    stderr: b"unable to open database file".to_vec(),
                })
            } else {
                Ok(Output {
                    code: Some(0),
                    stdout: if sql == SQL { vec![] } else { b"24".to_vec() },
                    stderr: vec![],
                })
            }
        })
        .unwrap();
        assert_eq!(result, (24, String::new()));
        assert_eq!(calls.len(), 3);
        assert!(calls[0].0.ends_with("?mode=ro"));
        assert!(calls[1].0.ends_with("?mode=ro&immutable=1"));
        assert!(calls[2]
            .1
            .contains("WHERE host_key IN ('cursor.com','.cursor.com')"));
        let mut calls = 0;
        assert!(database_with(Path::new("/synthetic/cookies"), |_, _| {
            calls += 1;
            Ok(Output {
                code: Some(1),
                stdout: vec![],
                stderr: b"database locked".to_vec(),
            })
        })
        .is_err());
        assert_eq!(calls, 1);
    }
    #[test]
    fn usage_consent_and_exit_codes_without_browser_or_keychain() {
        assert_eq!(
            authorized(true, false, || panic!("prompt"), || panic!("save")),
            Err(3)
        );
        assert_eq!(
            authorized(false, false, || false, || panic!("save")),
            Ok(false)
        );
        assert_eq!(authorized(false, false, || true, || Ok(())), Ok(true));
        assert_eq!(
            authorized(
                false,
                true,
                || panic!("repeat prompt"),
                || panic!("repeat save")
            ),
            Ok(true)
        );
        assert_eq!(
            finish_import(|| Err(()), |_| panic!("check"), |_| panic!("save")),
            1
        );
        assert_eq!(
            finish_import(
                || Ok(String::new()),
                |_| panic!("check"),
                |_| panic!("save")
            ),
            4
        );
        for code in [1, 4] {
            assert_eq!(
                finish_import(
                    || Ok("token=synthetic".into()),
                    |_| code,
                    |_| panic!("save failed validation")
                ),
                code
            );
        }
        assert_eq!(
            finish_import(|| Ok("token=synthetic".into()), |_| 0, |_| Err(())),
            1
        );
        assert_eq!(
            finish_import(|| Ok("token=synthetic".into()), |_| 0, |_| Ok(())),
            0
        );
        let path = directory().join("config.toml");
        save_consent(&path, true).unwrap();
        assert!(
            toml::from_str::<crate::config::Config>(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .ui
                .usage
                .cursor_login
        );
        save_consent(&path, false).unwrap();
        assert!(
            !toml::from_str::<crate::config::Config>(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .ui
                .usage
                .cursor_login
        );
    }
    #[test]
    fn usage_forget_all_three_lookup_states_and_errors() {
        for delete in [Some(0), Some(44), Some(1), None] {
            for state in [Presence::Present, Presence::Absent, Presence::Unknown] {
                let mut cleared = false;
                let code = forget(
                    || {
                        cleared = true;
                        Ok(())
                    },
                    delete,
                    || state,
                );
                assert_eq!(
                    code,
                    if delete == Some(0) || delete == Some(44) {
                        0
                    } else {
                        1
                    }
                );
                assert_eq!(
                    cleared,
                    delete == Some(0) || delete == Some(44) || state == Presence::Absent
                );
            }
        }
        assert_eq!(presence(Some(0)), Presence::Present);
        assert_eq!(presence(Some(44)), Presence::Absent);
        assert_eq!(presence(None), Presence::Unknown);
        assert_eq!(presence(Some(1)), Presence::Unknown);
        assert_eq!(
            forget(
                || Err(io::Error::other("synthetic")),
                Some(0),
                || panic!("lookup")
            ),
            1
        );
    }
    #[test]
    fn usage_secret_only_in_security_stdin_and_helper_deadline() {
        let start = Instant::now();
        assert!(execute(
            "/bin/sleep",
            &["5"],
            Some(&vec![b'x'; 1024 * 1024]),
            Instant::now() + Duration::from_millis(20)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(security_input("a=quote\"and\\slash").unwrap(),"add-generic-password -U -s \"herdr-cursor-usage\" -a \"herdr\" -w \"a=quote\\\"and\\\\slash\"\n");
        for secret in ["", "a\nb", "a\rb", "a\0b"] {
            assert!(security_input(secret).is_err());
        }
        assert!(execute(
            "/bin/sleep",
            &["5"],
            None,
            Instant::now() + Duration::from_millis(20)
        )
        .is_err());
    }
    #[test]
    fn usage_browser_order_default_then_profiles_chrome_then_brave() {
        let home = directory();
        for dir in [
            "Google/Chrome/Profile 2",
            "Google/Chrome/Default",
            "BraveSoftware/Brave-Browser/Default",
        ] {
            let profile = home.join("Library/Application Support").join(dir);
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join("Cookies"), b"synthetic").unwrap();
        }
        let found = browsers(&home);
        assert_eq!(found.len(), 3);
        assert!(found[0].path.ends_with("Default/Cookies"));
        assert_eq!(found[0].name, "Chrome");
        assert!(found[1].path.ends_with("Profile 2/Cookies"));
        assert_eq!(found[2].name, "Brave");
    }
}
