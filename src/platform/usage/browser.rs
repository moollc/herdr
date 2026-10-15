//! Default HTTPS browser identity. Desktop entries are data, never commands to run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DefaultBrowser {
    pub identity: Option<String>,
    pub name: Option<&'static str>,
    pub unsupported: Option<String>,
}
#[cfg(target_os = "macos")]
pub(crate) fn default_browser() -> DefaultBrowser {
    let identity = crate::platform::macos::default_browser_bundle_id();
    let name = identity.as_deref().and_then(|id| match id {
        "com.google.Chrome" => Some("Chrome"),
        "org.chromium.Chromium" => Some("Chromium"),
        "com.brave.Browser" => Some("Brave"),
        "com.microsoft.edgemac" => Some("Edge"),
        "com.vivaldi.Vivaldi" => Some("Vivaldi"),
        "company.thebrowser.Browser" => Some("Arc"),
        _ => None,
    });
    let unsupported = if name.is_none() {
        Some(format!(
            "Default browser {} is not supported for automatic import.",
            identity.as_deref().unwrap_or("unknown")
        ))
    } else {
        None
    };
    DefaultBrowser {
        identity,
        name,
        unsupported,
    }
}

#[cfg(any(target_os = "linux", test))]
mod desktop {
    use super::super::command::execute;
    use super::DefaultBrowser;
    use std::{
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    // Exact stable packaging IDs only. Sources are listed in BUILD-11.md.
    fn name(id: &str) -> Option<&'static str> {
        match id {
            "google-chrome.desktop" => Some("Chrome"),
            "chromium.desktop" | "chromium-browser.desktop" => Some("Chromium"),
            "brave-browser.desktop" | "com.brave.Browser.desktop" => Some("Brave"),
            "microsoft-edge.desktop" => Some("Edge"),
            "vivaldi-stable.desktop" => Some("Vivaldi"),
            _ => None,
        }
    }
    fn desktop_id(value: &str) -> Option<String> {
        let id = value.trim();
        (id.ends_with(".desktop")
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        .then(|| id.to_string())
    }
    fn association(contents: &str) -> Option<String> {
        let mut defaults = false;
        for line in contents.lines().map(str::trim) {
            if line.starts_with('[') {
                defaults = line == "[Default Applications]";
            }
            if defaults {
                if let Some((key, values)) = line.split_once('=') {
                    if key.trim() == "x-scheme-handler/https" {
                        return values.split(';').find_map(desktop_id);
                    }
                }
            }
        }
        None
    }
    fn paths(
        config: &Path,
        config_dirs: &[PathBuf],
        data: &Path,
        data_dirs: &[PathBuf],
        desktops: &[String],
    ) -> Vec<PathBuf> {
        let mut result = Vec::new();
        for root in std::iter::once(config.to_path_buf()).chain(config_dirs.iter().cloned()) {
            for desktop in desktops {
                result.push(root.join(format!("{desktop}-mimeapps.list")));
            }
            result.push(root.join("mimeapps.list"));
        }
        for root in std::iter::once(data.to_path_buf()).chain(data_dirs.iter().cloned()) {
            for desktop in desktops {
                result.push(
                    root.join("applications")
                        .join(format!("{desktop}-mimeapps.list")),
                );
            }
            result.push(root.join("applications/mimeapps.list"));
        }
        result
    }
    fn discover(
        mut helper: impl FnMut(&str, &[&str]) -> Option<String>,
        files: &[PathBuf],
        mut read: impl FnMut(&Path) -> Option<String>,
    ) -> DefaultBrowser {
        let identity = helper("xdg-settings", &["get", "default-web-browser"])
            .and_then(|s| desktop_id(&s))
            .or_else(|| {
                helper("xdg-mime", &["query", "default", "x-scheme-handler/https"])
                    .and_then(|s| desktop_id(&s))
            })
            .or_else(|| {
                files
                    .iter()
                    .find_map(|p| read(p).and_then(|s| association(&s)))
            });
        let name = identity.as_deref().and_then(name);
        let unsupported = Some(match (&identity, name) {
            (_, Some(name)) => format!("{name}: Linux automatic cookie import is deferred; use herdr usage cursor-login --paste."),
            (Some(id), _) => format!("Default browser {id} is not supported for automatic import; use herdr usage cursor-login --paste."),
            _ => "Default browser could not be determined; use herdr usage cursor-login --paste.".into(),
        });
        DefaultBrowser {
            identity,
            name,
            unsupported,
        }
    }
    pub(super) fn default_browser() -> DefaultBrowser {
        fn variable(name: &str, fallback: PathBuf) -> PathBuf {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or(fallback)
        }
        fn dirs(name: &str, fallback: &str) -> Vec<PathBuf> {
            std::env::split_paths(
                &std::env::var_os(name)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback.into()),
            )
            .filter(|p| p.is_absolute())
            .collect()
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .map(str::to_ascii_lowercase)
            .filter(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            })
            .collect::<Vec<_>>();
        let paths = paths(
            &variable("XDG_CONFIG_HOME", home.join(".config")),
            &dirs("XDG_CONFIG_DIRS", "/etc/xdg"),
            &variable("XDG_DATA_HOME", home.join(".local/share")),
            &dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
            &desktops,
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        discover(
            |program, args| {
                execute(program, args, None, deadline)
                    .ok()
                    .filter(|o| o.code == Some(0))
                    .and_then(|o| String::from_utf8(o.stdout).ok())
            },
            &paths,
            |p| {
                use std::io::Read;
                let file = std::fs::File::open(p).ok()?;
                let mut text = String::new();
                file.take(256 * 1024).read_to_string(&mut text).ok()?;
                Some(text)
            },
        )
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn usage_desktop_discovery_fallback_precedence_and_no_exec() {
            let files = paths(
                Path::new("/cfg"),
                &["/etc/xdg".into()],
                Path::new("/data"),
                &["/usr/share".into()],
                &["gnome".into()],
            );
            assert_eq!(files[0], Path::new("/cfg/gnome-mimeapps.list"));
            assert_eq!(files[1], Path::new("/cfg/mimeapps.list"));
            assert_eq!(
                files[4],
                Path::new("/data/applications/gnome-mimeapps.list")
            );
            let mut calls = Vec::new();
            let b = discover(
                |p, _| {
                    calls.push(p.to_string());
                    if p == "xdg-mime" {
                        Some("brave-browser.desktop\n".into())
                    } else {
                        None
                    }
                },
                &files,
                |_| panic!("no file fallback"),
            );
            assert_eq!(b.name, Some("Brave"));
            assert_eq!(calls, ["xdg-settings", "xdg-mime"]);
            let b = discover(
                |_, _| None,
                &files,
                |p| {
                    Some(if p == files[0] { "[Desktop Entry]\nExec=anything\n[Default Applications]\nx-scheme-handler/https=vivaldi-stable.desktop;" } else { "[Default Applications]\nx-scheme-handler/https=google-chrome.desktop;" }.into())
                },
            );
            assert_eq!(b.name, Some("Vivaldi"));
            let b = discover(|_, _| Some("firefox.desktop".into()), &[], |_| None);
            assert!(b.unsupported.unwrap().contains("firefox.desktop"));
            for id in [
                "google-chrome.desktop",
                "chromium.desktop",
                "chromium-browser.desktop",
                "microsoft-edge.desktop",
                "com.brave.Browser.desktop",
                "vivaldi-stable.desktop",
            ] {
                assert!(name(id).is_some());
            }
            for id in [
                "/tmp/google-chrome.desktop",
                "evil.desktop\nExec=foo",
                "$(run).desktop",
                "foo-google-chrome.desktop",
            ] {
                assert!(desktop_id(id).is_none() || name(id).is_none());
            }
        }
        #[test]
        fn usage_linux_runtime_discovery_compiles_on_mac_with_fake_helpers() {
            // Exercise the same entrypoint's helpers through injected discovery; do not inspect this Mac's desktop.
            let b = discover(|_, _| None, &[], |_| None);
            assert!(b.identity.is_none());
            let _entrypoint: fn() -> DefaultBrowser = default_browser;
        }
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn default_browser() -> DefaultBrowser {
    desktop::default_browser()
}
