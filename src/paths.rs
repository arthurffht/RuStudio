//! Per-user folders on Windows via `dirs` (`%LOCALAPPDATA%` for data, state
//! and cache; `%APPDATA%` for config), matching `rustudio-account`.

use std::path::PathBuf;

/// The user's home folder ("." when it can't be found).
pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user local data (`%LOCALAPPDATA%`).
pub fn data() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(home)
}

/// Per-user roaming configuration (`%APPDATA%`).
pub fn config() -> PathBuf {
    dirs::config_dir().unwrap_or_else(home)
}

/// State that outlives a run (`%LOCALAPPDATA%`).
pub fn state() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(home)
}

/// Caches (`%LOCALAPPDATA%`).
pub fn cache() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir)
}

/// Where screenshots go: the user's Pictures folder.
pub fn pictures() -> PathBuf {
    dirs::picture_dir().unwrap_or_else(|| home().join("Pictures"))
}

/// Roblox Studio installed on Windows (`%LOCALAPPDATA%\Roblox\Versions\version-*`),
/// newest modified first, holding both `content` and `ExtraContent\scripts\PlayerScripts`.
pub fn installed_studio() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(data);
    let versions = local.join("Roblox").join("Versions");
    let rd = std::fs::read_dir(&versions).ok()?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("version-"))
                && p.join("content").is_dir()
                && p.join("ExtraContent/scripts/PlayerScripts").is_dir()
        })
        .map(|p| {
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// A data file that ships with rustudio (`api-dump.json`), next to the
/// executable or in the source tree it was built from (`cargo run`).
pub fn resource(name: &str) -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        .and_then(|e| e.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let p = dir.join(name);
        if p.is_file() {
            return p;
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_folders_exist() {
        assert_eq!(data(), dirs::data_local_dir().unwrap_or_else(home));
        assert_eq!(config(), dirs::config_dir().unwrap_or_else(home));
        assert_eq!(state(), dirs::data_local_dir().unwrap_or_else(home));
    }

    #[test]
    fn api_dump_found() {
        assert!(resource("api-dump.json").is_file());
    }
}
