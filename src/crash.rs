//! Crash reports: a panic (any thread) or an unhandled Windows SEH exception
//! writes `%LOCALAPPDATA%\rustudio\crash\<timestamp>-<pid>.txt` with a
//! backtrace, and the next start lists any report it hasn't mentioned yet in
//! Output. A crash is never silent, even when rustudio was started with no
//! console.

use std::backtrace::Backtrace;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where reports go (`%LOCALAPPDATA%\rustudio\crash`).
pub fn dir() -> PathBuf {
    crate::paths::cache().join("rustudio").join("crash")
}

/// Pre-computed report path for this process.
static CRASH_PATH: OnceLock<PathBuf> = OnceLock::new();
/// The first fatal event writes the report; a panic that then aborts must
/// not overwrite its own report.
static WRITTEN: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Inside `recoverable`: a panic here is caught and handled, not a crash.
    static RECOVERING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run `f`, catching a panic in it (the caller falls back). Such a panic is
/// expected, so it writes no crash report.
pub fn recoverable<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    RECOVERING.with(|r| r.set(r.get() + 1));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    RECOVERING.with(|r| r.set(r.get() - 1));
    out
}

fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Command-line arguments with anything that looks like a credential removed.
fn args_line() -> String {
    std::env::args()
        .map(|a| {
            if a.contains("ROBLOSECURITY") || a.contains("WARNING:-DO-NOT-SHARE") || a.len() > 200 {
                "<redacted>".to_string()
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn header(kind: &str) -> String {
    format!(
        "rustudio {} ({}) crashed: {kind}\npid {}\nargs: {}\n\n",
        env!("CARGO_PKG_VERSION"),
        option_env!("RUSTUDIO_GIT").unwrap_or("unknown commit"),
        std::process::id(),
        args_line()
    )
}

fn write_report(body: &str) -> Option<PathBuf> {
    let d = dir();
    std::fs::create_dir_all(&d).ok()?;
    let path = d.join(format!("{}-{}.txt", stamp(), std::process::id()));
    let mut f = std::fs::File::create(&path).ok()?;
    f.write_all(body.as_bytes()).ok()?;
    Some(path)
}

/// Install the panic hook and the Windows SEH unhandled exception filter.
/// Call first in main.
pub fn install() {
    let path = dir().join(format!("{}-{}.txt", stamp(), std::process::id()));
    let _ = CRASH_PATH.set(path);

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if RECOVERING.with(|r| r.get()) > 0 {
            return;
        }
        // Write first, then let the default hook print to stderr as before.
        if !WRITTEN.swap(true, Ordering::SeqCst) {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>").to_string();
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".into());
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            let bt = Backtrace::force_capture();
            let body = format!(
                "{}panic in thread '{name}' at {loc}:\n{msg}\n\nbacktrace:\n{bt}\n",
                header("panic")
            );
            if let Some(p) = write_report(&body) {
                eprintln!("rustudio: crash report written to {}", p.display());
            }
        }
        default_hook(info);
    }));

    let _ = std::fs::create_dir_all(dir());
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(
            unhandled_exception_filter,
        ));
    }
}

#[cfg(windows)]
unsafe extern "system" fn unhandled_exception_filter(
    info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    if !WRITTEN.swap(true, Ordering::SeqCst) {
        let (code, addr) = if !info.is_null() {
            let rec = unsafe { (*info).ExceptionRecord };
            if !rec.is_null() {
                unsafe { ((*rec).ExceptionCode as u32, (*rec).ExceptionAddress as usize) }
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };
        let kind = format!("Windows SEH exception {code:#010x} at {addr:#x}");
        let bt = Backtrace::force_capture();
        let body = format!(
            "rustudio crashed: fatal {kind}\n\n{}backtrace:\n{bt}\n",
            header(&kind)
        );
        if let Some(p) = CRASH_PATH.get() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, body.as_bytes());
        } else {
            let _ = write_report(&body);
        }
    }
    windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_SEARCH
}

/// Reports from earlier runs not yet shown, oldest first, as Output lines.
/// Each is shown once (`.shown` remembers them).
pub fn unseen_reports() -> Vec<String> {
    let d = dir();
    let seen_file = d.join(".shown");
    let seen = std::fs::read_to_string(&seen_file).unwrap_or_default();
    let mine = CRASH_PATH
        .get()
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    let mut found: Vec<PathBuf> = std::fs::read_dir(&d)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.len() > 16
                && n.ends_with(".txt")
                && n.as_bytes()[..8].iter().all(u8::is_ascii_digit)
                && n.as_bytes()[8] == b'-'
                && !seen.lines().any(|l| l == n)
                && p.to_str() != Some(mine)
        })
        .collect();
    found.sort();
    let mut lines = Vec::new();
    let mut add = String::new();
    for p in found {
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        // The first line after the header that says what happened.
        let what = text
            .lines()
            .find(|l| l.starts_with("panic in thread") || l.starts_with("rustudio crashed: fatal"))
            .unwrap_or("crash")
            .to_string();
        let msg = text
            .lines()
            .skip_while(|l| !l.starts_with("panic in thread"))
            .nth(1)
            .map(|m| format!(" — {m}"))
            .unwrap_or_default();
        lines.push(format!(
            "rustudio crashed last time: {what}{msg}. Report: {}",
            p.display()
        ));
        if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
            add.push_str(n);
            add.push('\n');
        }
    }
    if !add.is_empty() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&seen_file) {
            let _ = f.write_all(add.as_bytes());
        }
    }
    lines
}
