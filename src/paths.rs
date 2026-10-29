use std::path::{Path, PathBuf};

/// Where config, logs, the runner download cache and host-backend work dirs live.
pub fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        // The Windows service runs as LocalSystem by default, so per-user AppData is the wrong place.
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        PathBuf::from(base).join("home-runner")
    } else {
        home_dir().join(".home-runner")
    }
}

pub fn default_config_path() -> PathBuf {
    default_data_dir().join("config.toml")
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expands a leading `~/` so config paths can be written portably.
pub fn expand_home(path: &Path) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix("~/")) {
        Some(rest) => home_dir().join(rest),
        None => path.to_path_buf(),
    }
}
