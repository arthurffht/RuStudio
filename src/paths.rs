//! Per-user folders on Windows via `dirs` (`%LOCALAPPDATA%` for data, state
//! and cache; `%APPDATA%` for config), matching `rustudio-account`.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

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

fn reg_client_exe_dir(hive: windows::Win32::System::Registry::HKEY, subkey: &str) -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};

    let subkey_w: Vec<u16> = subkey.encode_utf16().chain([0]).collect();
    let value_w: Vec<u16> = "clientExe\0".encode_utf16().collect();
    let mut bytes: u32 = 0;
    // SAFETY: `subkey_w` and `value_w` are NUL-terminated UTF-16 strings; query required byte size first.
    let err = unsafe {
        RegGetValueW(
            hive,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if err != ERROR_SUCCESS || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize + 1) / 2];
    let mut out_bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` has capacity `out_bytes` bytes.
    let err = unsafe {
        RegGetValueW(
            hive,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut out_bytes),
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let raw = String::from_utf16_lossy(&buf[..len]);
    let exe = raw.trim().trim_matches('"');
    if exe.is_empty() {
        return None;
    }
    Path::new(exe).parent().map(Path::to_path_buf)
}

fn version_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(data);
    roots.push(local.join("Roblox").join("Versions"));
    if let Some(pf86) = std::env::var_os("ProgramFiles(x86)").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(pf86).join("Roblox").join("Versions"));
    }
    roots
}

fn scan_versions(pred: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for versions in version_roots() {
        let Ok(rd) = std::fs::read_dir(&versions) else { continue };
        for p in rd.flatten().map(|e| e.path()) {
            if p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("version-"))
                && pred(&p)
            {
                let mtime = std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((mtime, p));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

fn discover_studio() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let is_studio = |p: &Path| {
        p.join("content").is_dir() && p.join("ExtraContent/scripts/PlayerScripts").is_dir()
    };
    for (hive, subkey) in [
        (HKEY_CURRENT_USER, r"Software\ROBLOX Corporation\Environments\roblox-studio"),
        (HKEY_LOCAL_MACHINE, r"Software\WOW6432Node\ROBLOX Corporation\Environments\roblox-studio"),
    ] {
        if let Some(dir) = reg_client_exe_dir(hive, subkey).filter(|d| is_studio(d)) {
            return Some(dir);
        }
    }
    scan_versions(is_studio)
}

fn discover_player() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let has_content = |p: &Path| p.join("content").is_dir();
    for (hive, subkey) in [
        (HKEY_CURRENT_USER, r"Software\ROBLOX Corporation\Environments\roblox-player"),
        (HKEY_LOCAL_MACHINE, r"Software\WOW6432Node\ROBLOX Corporation\Environments\roblox-player"),
    ] {
        if let Some(dir) = reg_client_exe_dir(hive, subkey).filter(|d| has_content(d)) {
            return Some(dir);
        }
    }
    scan_versions(|p| has_content(p) && p.join("RobloxPlayerBeta.exe").is_file())
}

fn cached_install(
    slot: &OnceLock<RwLock<(u64, Option<PathBuf>)>>,
    discover: fn() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let gen = crate::studio_content::content_generation();
    let lock = slot.get_or_init(|| RwLock::new((gen, discover())));
    if let Ok(guard) = lock.read() {
        if guard.0 == gen {
            return guard.1.clone();
        }
    }
    let fresh = discover();
    if let Ok(mut guard) = lock.write() {
        *guard = (gen, fresh.clone());
    }
    fresh
}

/// Roblox Studio installed on Windows (`Environments\roblox-studio` registry
/// or `Roblox\Versions\version-*`), holding `content` and
/// `ExtraContent\scripts\PlayerScripts`. Cached across calls.
pub fn installed_studio() -> Option<PathBuf> {
    static CACHE: OnceLock<RwLock<(u64, Option<PathBuf>)>> = OnceLock::new();
    cached_install(&CACHE, discover_studio)
}

/// Roblox Player installed on Windows (`Environments\roblox-player` registry
/// or `Roblox\Versions\version-*` with `RobloxPlayerBeta.exe` and `content`).
/// Cached across calls.
pub fn installed_player() -> Option<PathBuf> {
    static CACHE: OnceLock<RwLock<(u64, Option<PathBuf>)>> = OnceLock::new();
    cached_install(&CACHE, discover_player)
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
