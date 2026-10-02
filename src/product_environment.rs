use std::path::{Path, PathBuf};

// All product environment reads share this precedence, including process-only
// worker entry points. An explicitly set new name (even empty) blocks fallback.
pub fn product_env<T>(key: &str, mut read: impl FnMut(&str) -> Option<T>) -> Option<T> {
    let suffix = key
        .strip_prefix("HERDR_FARM_")
        .or_else(|| key.strip_prefix("HERDR_PROJECTS_"));
    match suffix {
        Some(suffix) => read(&format!("HERDR_FARM_{suffix}"))
            .or_else(|| read(&format!("HERDR_PROJECTS_{suffix}"))),
        None => read(key),
    }
}

pub fn product_var_os(key: &str) -> Option<std::ffi::OsString> {
    product_env(key, |name| std::env::var_os(name))
}

pub fn config_dir_for_home(home: &Path) -> PathBuf {
    let new = home.join(".config/herdr-farm");
    let old = home.join(".config/herdr-projects");
    if new.join("config.toml").exists() || !old.exists() {
        new
    } else {
        old
    }
}
