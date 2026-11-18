//! Windows platform helpers: process statistics, Recycle Bin, system fonts, URL launch.

use std::path::Path;

/// This process's resident memory (working set) in bytes.
pub fn resident_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: zeroed counters struct with `cb` initialized to its byte size.
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    (ok != 0).then_some(pmc.WorkingSetSize as u64)
}

/// The 1-minute load average as text ("?" on Windows).
pub fn load_average() -> String {
    "?".into()
}

/// Moves `path` to the Windows Recycle Bin so Restore works.
pub fn native_trash(path: &Path) -> Option<Result<(), String>> {
    Some(recycle(path))
}

/// The shell's delete with undo: the item goes to the Recycle Bin. Where the
/// drive has no Recycle Bin (some network and removable drives) the shell
/// would delete it outright, so it asks first (FOF_WANTNUKEWARNING), and
/// answering No leaves the item where it is.
fn recycle(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FOF_WANTNUKEWARNING, FO_DELETE,
        SHFILEOPSTRUCTW,
    };
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    // pFrom is a list of paths, each ended by a NUL, the list by another.
    let from: Vec<u16> = abs.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_WANTNUKEWARNING) as u16,
        ..Default::default()
    };
    // SAFETY: `op` is fully initialised and `from` outlives the call.
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(format!("the shell refused (error {r:#x})"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Validates that `url` uses an `http://` or `https://` scheme and has no NUL bytes.
fn valid_web_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://")) && !url.contains('\0') {
        Some(url)
    } else {
        None
    }
}

/// Opens an `http://` or `https://` URL in the default Windows browser via `ShellExecuteW`.
pub fn open_url(url: &str) {
    let Some(url) = valid_web_url(url) else { return };
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().chain([0]).collect();
    // SAFETY: `op` and `file` are NUL-terminated UTF-16 strings that outlive the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL.0,
        );
    }
}

/// System fonts for the symbols and scripts rustudio's own fonts lack, in
/// the order to try them.
pub fn fallback_font_files() -> &'static [&'static str] {
    &[
        r"C:\Windows\Fonts\seguisym.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\segoeuib.ttf",
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\YuGothR.ttc",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_stats() {
        let rss = resident_bytes().expect("resident memory");
        assert!(rss > (1 << 20), "{rss}");
    }

    #[test]
    fn url_validation_allows_http_and_https_only() {
        assert_eq!(valid_web_url("https://create.roblox.com/store"), Some("https://create.roblox.com/store"));
        assert_eq!(valid_web_url("  http://example.com/a?b=1  "), Some("http://example.com/a?b=1"));
        assert_eq!(valid_web_url("file:///C:/Windows/win.ini"), None);
        assert_eq!(valid_web_url("cmd://calc"), None);
        assert_eq!(valid_web_url("javascript:alert(1)"), None);
        assert_eq!(valid_web_url("https://example.com\0evil"), None);
    }

    #[test]
    fn trash_goes_to_recycle_bin() {
        let dir = std::env::temp_dir().join(format!("rustudio-recycle-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let f = dir.join("gone.txt");
        std::fs::write(&f, "x").unwrap();
        for p in [f, dir.join("folder")] {
            match recycle(&p) {
                Ok(()) => assert!(!p.exists(), "{}", p.display()),
                // A CI runner's profile may have no Recycle Bin.
                Err(e) => eprintln!("Recycle Bin unavailable here: {e}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
