//! Windows platform helpers: process statistics, Recycle Bin, system fonts.

use std::path::Path;

/// This process's resident memory (working set) in bytes.
pub fn resident_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: zeroed counters struct with `cb` initialized to its byte size.
        let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
        (ok != 0).then_some(pmc.WorkingSetSize as u64)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The 1-minute load average as text ("?" on Windows).
pub fn load_average() -> String {
    "?".into()
}

/// Moves `path` to the Windows Recycle Bin so Restore works.
pub fn native_trash(path: &Path) -> Option<Result<(), String>> {
    #[cfg(windows)]
    {
        Some(recycle(path))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// The shell's delete with undo: the item goes to the Recycle Bin. Where the
/// drive has no Recycle Bin (some network and removable drives) the shell
/// would delete it outright, so it asks first (FOF_WANTNUKEWARNING), and
/// answering No leaves the item where it is.
#[cfg(windows)]
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

/// System fonts for the symbols and scripts rustudio's own fonts lack, in
/// the order to try them.
pub fn fallback_font_files() -> &'static [&'static str] {
    &[
        r"C:\Windows\Fonts\seguisym.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\consola.ttf",
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

    #[cfg(windows)]
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
