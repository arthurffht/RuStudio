//! Studio-parity graphics quality profiles and adaptive Frame Rate Manager (FRM).

use crate::d3d11::GpuCaps;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QualityProfile {
    /// Quality level in `1..=21` (matches Roblox `Enum.QualityLevel`).
    pub level: u8,
    pub auto_mode: bool,
    pub msaa_samples: u32,
    pub shadow_atlas_side: u32,
    pub shadow_cascades: usize,
    pub enable_shadows: bool,
    pub enable_ssao: bool,
    pub enable_dof: bool,
    pub enable_bloom: bool,
    pub enable_clouds: bool,
    pub max_local_shadows: usize,
    pub lod_bias: f32,
}

impl QualityProfile {
    pub fn from_level(level: u8, caps: &GpuCaps, auto_mode: bool) -> Self {
        let lvl = level.clamp(1, 21);
        let wants_msaa = if lvl >= 16 { 4 } else { 1 };
        let msaa_samples = if wants_msaa > 1 && caps.msaa_fp16_max_samples >= 4 && !caps.is_uma {
            4
        } else {
            1
        };
        Self {
            level: lvl,
            auto_mode,
            msaa_samples,
            shadow_atlas_side: if lvl >= 14 && !caps.is_uma {
                2048
            } else if lvl >= 4 {
                1024
            } else {
                512
            },
            shadow_cascades: if lvl >= 16 {
                4
            } else if lvl >= 10 {
                3
            } else if lvl >= 4 {
                2
            } else {
                1
            },
            enable_shadows: lvl >= 4,
            enable_ssao: lvl >= 12 && (!auto_mode || !caps.is_uma),
            enable_dof: lvl >= 14 && (!auto_mode || !caps.is_uma),
            enable_bloom: lvl >= 6,
            enable_clouds: lvl >= 4,
            max_local_shadows: if lvl >= 16 && !caps.is_uma {
                16
            } else if lvl >= 8 {
                4
            } else {
                0
            },
            lod_bias: if lvl >= 14 {
                1.0
            } else if lvl >= 8 {
                1.25
            } else {
                1.75
            },
        }
    }

    pub fn recommended_for_caps(caps: &GpuCaps) -> u8 {
        if caps.is_uma || caps.dedicated_vram_mb <= 512 || caps.msaa_fp16_max_samples < 4 {
            5
        } else if caps.dedicated_vram_mb < 2048 {
            12
        } else {
            21
        }
    }

    pub fn resolve(
        caps: &GpuCaps,
        quality_setting: u8,
        frm_enabled: bool,
        frm_level: Option<u8>,
    ) -> Self {
        let auto_mode = quality_setting == 0 || frm_enabled;
        let level = if quality_setting == 0 {
            frm_level.unwrap_or_else(|| Self::recommended_for_caps(caps))
        } else if frm_enabled {
            frm_level.unwrap_or(quality_setting)
        } else {
            quality_setting
        };
        Self::from_level(level, caps, auto_mode)
    }
}

/// Hysteresis-based Frame Rate Manager that tracks smoothed GPU/CPU frame time
/// and steps `QualityLevel` up or down to maintain interactive frame rates.
pub struct FrameRateManager {
    level: u8,
    ema_ms: f32,
    frames_since_change: u32,
}

impl FrameRateManager {
    pub fn new(caps: &GpuCaps) -> Self {
        Self {
            level: QualityProfile::recommended_for_caps(caps),
            ema_ms: 16.6,
            frames_since_change: 0,
        }
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn smoothed_ms(&self) -> f32 {
        self.ema_ms
    }

    pub fn set_level(&mut self, level: u8) {
        self.level = level.clamp(1, 21);
        self.frames_since_change = 0;
    }

    /// Record a frame duration in milliseconds and return the updated target quality level.
    pub fn observe_frame(&mut self, frame_ms: f32) -> u8 {
        self.ema_ms = self.ema_ms * 0.9 + frame_ms.clamp(0.5, 500.0) * 0.1;
        self.frames_since_change = self.frames_since_change.saturating_add(1);
        if self.frames_since_change >= 45 {
            if self.ema_ms > 22.0 && self.level > 1 {
                self.level = self.level.saturating_sub(2).max(1);
                self.frames_since_change = 0;
            } else if self.ema_ms < 10.5 && self.level < 21 {
                self.level = (self.level + 1).min(21);
                self.frames_since_change = 0;
            }
        }
        self.level
    }
}
