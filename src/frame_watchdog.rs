//! Independent frame progress diagnostics; never holds a lock across editor work.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

struct Progress {
    completed: Instant,
    stage: &'static str,
    pending: String,
    active: bool,
}
impl Progress {
    fn stalled(&self, now: Instant) -> bool {
        self.active && now.saturating_duration_since(self.completed) > Duration::from_secs(5)
    }
}
pub struct Watchdog {
    state: Arc<Mutex<Progress>>,
    stop: Arc<AtomicBool>,
}
impl Watchdog {
    pub fn new(ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(Progress {
            completed: Instant::now(),
            stage: "startup",
            pending: String::new(),
            active: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, done) = (state.clone(), stop.clone());
        std::thread::Builder::new().name("frame watchdog".into()).spawn(move || {
            let mut reported = false;
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let p = s.lock().unwrap_or_else(|e| e.into_inner());
                let stalled = p.stalled(Instant::now());
                if stalled && !reported {
                    eprintln!("rustudio frame stall: {:.1}s since completed frame; last completed stage={}; pending waits={}", p.completed.elapsed().as_secs_f32(), p.stage, p.pending);
                }
                reported = stalled;
                // A fresh cross-thread repaint event recovers an event loop whose
                // previous redraw was consumed without delivering another frame.
                if stalled { ctx.request_repaint(); }
            }
        }).expect("frame watchdog");
        Self { state, stop }
    }
    pub fn stage(&self, stage: &'static str, pending: &str) {
        let mut p = self.state.lock().unwrap_or_else(|e| e.into_inner());
        p.stage = stage;
        p.pending = pending.into();
    }
    pub fn begin(&self, active: bool) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active = active;
        self.stage("previous frame", "asset polling");
    }
    pub fn complete(&self) {
        let mut p = self.state.lock().unwrap_or_else(|e| e.into_inner());
        p.completed = Instant::now();
        p.stage = "UI frame";
        p.pending = "D3D11 GPU submission/presentation or next redraw event".into();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_stall_only_when_frames_are_expected() {
        let now = Instant::now();
        let mut p = Progress {
            completed: now,
            stage: "UI frame",
            pending: String::new(),
            active: true,
        };
        assert!(!p.stalled(now + Duration::from_secs(5)));
        assert!(p.stalled(now + Duration::from_secs(6)));
        p.active = false;
        assert!(!p.stalled(now + Duration::from_secs(60)));
        p.active = true;
        p.completed = now + Duration::from_secs(60);
        assert!(!p.stalled(now + Duration::from_secs(60)));
    }
}
