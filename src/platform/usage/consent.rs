use std::{fs, io, path::Path};
pub(super) fn consent() -> bool {
    crate::config::Config::load().config.ui.usage.cursor_login
}
pub(super) fn save_consent(path: &Path, enabled: bool) -> io::Result<()> {
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
