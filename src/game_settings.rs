//! The in-game menu's settings, kept between playtests and runs, as
//! Roblox keeps UserGameSettings per user: Volume (MasterVolume), Camera
//! Sensitivity (MouseSensitivity) and Camera Inverted (CameraYInverted).
//! Without this every Play started at full volume again.
//!
//! `$XDG_CONFIG_HOME/rustudio/game-settings.json`. Only runs a person
//! drives read or write it (`Harness::plain`): tests and harness runs keep
//! the defaults and never touch the user's file.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

static GRAPHICS_QUALITY: AtomicU8 = AtomicU8::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// MasterVolume, 0..1.
    pub volume: Option<f32>,
    /// MouseSensitivity.
    pub sensitivity: Option<f64>,
    pub inverted: Option<bool>,
    /// UserGameSettings.SavedQualityLevel (`0` = Automatic, `1..=10` = manual).
    pub graphics_quality: u8,
}

pub fn graphics_quality() -> u8 {
    GRAPHICS_QUALITY.load(Ordering::Relaxed).min(10)
}

pub fn set_graphics_quality_runtime(q: u8) {
    GRAPHICS_QUALITY.store(q.min(10), Ordering::Relaxed);
}

pub fn set_graphics_quality(q: u8) {
    let clamped = q.min(10);
    GRAPHICS_QUALITY.store(clamped, Ordering::Relaxed);
    update(|s| s.graphics_quality = clamped);
}

pub fn step_graphics_quality(delta: i8) -> u8 {
    let cur = graphics_quality() as i16;
    let next = (cur + delta as i16).clamp(1, 10) as u8;
    set_graphics_quality(next);
    next
}

fn path() -> std::path::PathBuf {
    crate::paths::config().join("rustudio").join("game-settings.json")
}

pub fn load() -> GameSettings {
    let s: GameSettings = std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    GRAPHICS_QUALITY.store(s.graphics_quality.min(10), Ordering::Relaxed);
    s
}

/// Changes the saved settings (read, change, write back).
pub fn update(f: impl FnOnce(&mut GameSettings)) {
    let mut s = load();
    let before = s;
    f(&mut s);
    if s == before {
        return;
    }
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&s) {
        // Written beside it, then renamed: a crash mid-write keeps the old.
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

impl GameSettings {
    /// Client Luau that puts the saved values on UserGameSettings.
    pub fn client_lua(&self) -> Vec<String> {
        let ugs = "UserSettings():GetService(\"UserGameSettings\")";
        let mut v = Vec::new();
        if let Some(s) = self.sensitivity.filter(|s| s.is_finite()) {
            v.push(format!("{ugs}.MouseSensitivity = {s}"));
        }
        if let Some(f) = self.volume.filter(|f| f.is_finite()) {
            v.push(format!("{ugs}.MasterVolume = {}", f.clamp(0.0, 1.0)));
        }
        if let Some(on) = self.inverted {
            v.push(format!("{ugs}.CameraYInverted = {on}"));
        }
        if self.graphics_quality > 0 {
            let q = self.graphics_quality.min(10);
            v.push(format!(
                "{ugs}.SavedQualityLevel = Enum.SavedQualitySetting.QualityLevel{q}"
            ));
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_builds_the_client_lua() {
        let s = GameSettings { volume: Some(0.2), sensitivity: Some(0.37), inverted: Some(true), graphics_quality: 0 };
        let back: GameSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        // Older / partial files load with the rest unset.
        let partial: GameSettings = serde_json::from_str(r#"{"volume":0.3}"#).unwrap();
        assert_eq!(partial, GameSettings { volume: Some(0.3), ..Default::default() });
        let ugs = "UserSettings():GetService(\"UserGameSettings\")";
        assert_eq!(
            s.client_lua(),
            [
                format!("{ugs}.MouseSensitivity = 0.37"),
                format!("{ugs}.MasterVolume = 0.2"),
                format!("{ugs}.CameraYInverted = true"),
            ]
        );
        let manual = GameSettings { graphics_quality: 4, ..s };
        let manual_back: GameSettings = serde_json::from_str(&serde_json::to_string(&manual).unwrap()).unwrap();
        assert_eq!(manual_back, manual);
        assert_eq!(
            manual.client_lua(),
            [
                format!("{ugs}.MouseSensitivity = 0.37"),
                format!("{ugs}.MasterVolume = 0.2"),
                format!("{ugs}.CameraYInverted = true"),
                format!("{ugs}.SavedQualityLevel = Enum.SavedQualitySetting.QualityLevel4"),
            ]
        );
        assert!(GameSettings::default().client_lua().is_empty());
    }
}
