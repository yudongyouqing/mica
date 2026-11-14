pub mod defaults;
pub mod palette;
pub mod parse;
pub mod settings;
pub mod theme;

/// Shared by the GUI and configuration CLI commands. Existing Windows location
/// remains compatible; no GUI dependencies or filesystem mutation here.
pub fn user_config_path() -> std::path::PathBuf {
    match std::env::var_os("APPDATA").filter(|s| !s.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir).join("mica").join("config"),
        None => std::path::PathBuf::from(".mica").join("config"),
    }
}
