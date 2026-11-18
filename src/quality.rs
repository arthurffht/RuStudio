//! Studio-parity graphics quality profiles and adaptive Frame Rate Manager (FRM).

use crate::d3d11::GpuCaps;
use crate::mech;

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
    pub max_anisotropy: u32,
}

impl QualityProfile {
    pub fn from_level(level: u8, caps: &GpuCaps, auto_mode: bool) -> Self {
        let lvl = level.clamp(mech::QUALITY_LEVEL_MIN, mech::QUALITY_LEVEL_MAX);
        let wants_msaa = if lvl >= mech::QUALITY_MSAA_MIN_LEVEL {
            mech::QUALITY_MSAA_SAMPLES
        } else {
            1
        };
        let msaa_samples =
            if wants_msaa > 1 && caps.msaa_fp16_max_samples >= mech::QUALITY_MSAA_SAMPLES && !caps.is_uma {
                mech::QUALITY_MSAA_SAMPLES
            } else {
                1
            };
        let low_tier = lvl <= mech::QUALITY_LOW_SHADER_MAX_LEVEL;
        let max_anisotropy = if low_tier {
            mech::QUALITY_ANISO_LOW
        } else if lvl >= mech::QUALITY_ANISO_HIGH_MIN_LEVEL && !caps.is_uma {
            (mech::TEXTURE_ANISOTROPY as u32).min(caps.max_anisotropy).max(1)
        } else if lvl >= mech::QUALITY_ANISO_MID_MIN_LEVEL && lvl > mech::QUALITY_ANISO_LOW_MAX_LEVEL {
            mech::QUALITY_ANISO_MID.min(caps.max_anisotropy).max(1)
        } else {
            mech::QUALITY_ANISO_LOW
        };
        Self {
            level: lvl,
            auto_mode,
            msaa_samples,
            shadow_atlas_side: if lvl >= mech::QUALITY_HIGH_SHADOW_MIN_LEVEL && !caps.is_uma {
                mech::QUALITY_SHADOW_ATLAS_HIGH
            } else if lvl >= mech::QUALITY_SHADOWS_MIN_LEVEL {
                mech::QUALITY_SHADOW_ATLAS_MID
            } else {
                mech::QUALITY_SHADOW_ATLAS_LOW
            },
            shadow_cascades: if low_tier {
                0
            } else if lvl >= mech::QUALITY_CASCADES_4_MIN_LEVEL {
                4
            } else if lvl >= mech::QUALITY_CASCADES_3_MIN_LEVEL {
                3
            } else if lvl >= mech::QUALITY_SHADOWS_MIN_LEVEL {
                2
            } else {
                1
            },
            enable_shadows: !low_tier && lvl >= mech::QUALITY_SHADOWS_MIN_LEVEL,
            enable_ssao: !low_tier && lvl >= mech::QUALITY_SSAO_MIN_LEVEL && (!auto_mode || !caps.is_uma),
            enable_dof: !low_tier && lvl >= mech::QUALITY_DOF_MIN_LEVEL && (!auto_mode || !caps.is_uma),
            enable_bloom: !low_tier && lvl >= mech::QUALITY_BLOOM_MIN_LEVEL,
            enable_clouds: !low_tier && lvl >= mech::QUALITY_SHADOWS_MIN_LEVEL,
            max_local_shadows: if low_tier {
                0
            } else if lvl >= mech::QUALITY_MSAA_MIN_LEVEL && !caps.is_uma {
                mech::QUALITY_MAX_LOCAL_SHADOWS_HIGH
            } else if lvl >= mech::QUALITY_LOCAL_SHADOWS_MIN_LEVEL {
                mech::QUALITY_MAX_LOCAL_SHADOWS_MID
            } else {
                0
            },
            lod_bias: if lvl >= mech::QUALITY_LOD_FULL_MIN_LEVEL {
                1.0
            } else if lvl >= mech::QUALITY_LOCAL_SHADOWS_MIN_LEVEL {
                1.25
            } else {
                1.75
            },
            max_anisotropy,
        }
    }

    pub fn recommended_for_caps(caps: &GpuCaps) -> u8 {
        if caps.is_uma
            || caps.dedicated_vram_mb <= mech::QUALITY_LOW_VRAM_MB
            || caps.msaa_fp16_max_samples < mech::QUALITY_MSAA_SAMPLES
        {
            mech::QUALITY_UMA_DEFAULT_LEVEL
        } else if caps.dedicated_vram_mb < mech::QUALITY_MID_VRAM_MB {
            mech::QUALITY_MID_DEFAULT_LEVEL
        } else {
            mech::QUALITY_LEVEL_MAX
        }
    }

    pub fn resolve(
        caps: &GpuCaps,
        quality_setting: u8,
        frm_enabled: bool,
        frm_level: Option<u8>,
    ) -> Self {
        let auto_mode = quality_setting == 0 || frm_enabled;
        let raw_level = if quality_setting == 0 {
            frm_level.unwrap_or_else(|| Self::recommended_for_caps(caps))
        } else if frm_enabled {
            frm_level.unwrap_or(quality_setting).min(quality_setting)
        } else {
            quality_setting
        };
        let level = if auto_mode && caps.is_uma {
            raw_level.min(mech::QUALITY_MID_DEFAULT_LEVEL)
        } else {
            raw_level
        };
        Self::from_level(level, caps, auto_mode)
    }

    /// Maps `UserGameSettings.SavedQualityLevel` (`0` = Automatic, `1..=10`) to `1..=21`.
    pub fn saved_to_21(saved: u8) -> u8 {
        match saved {
            0 => 0,
            1 => 1,
            2 => 3,
            3 => 5,
            4 => 7,
            5 => 10,
            6 => 12,
            7 => 14,
            8 => 16,
            9 => 18,
            _ => 21,
        }
    }
}

/// Hysteresis-based Frame Rate Manager that tracks smoothed GPU/CPU frame time
/// and steps `QualityLevel` up or down to maintain interactive frame rates.
pub struct FrameRateManager {
    level: u8,
    max_level: u8,
    ema_ms: f32,
    frames_since_change: u32,
}

impl FrameRateManager {
    pub fn new(caps: &GpuCaps) -> Self {
        let max_level = if caps.is_uma {
            mech::QUALITY_MID_DEFAULT_LEVEL
        } else {
            mech::QUALITY_LEVEL_MAX
        };
        Self {
            level: QualityProfile::recommended_for_caps(caps).min(max_level),
            max_level,
            ema_ms: mech::FRM_TARGET_INITIAL_MS,
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
        self.level = level.clamp(mech::QUALITY_LEVEL_MIN, mech::QUALITY_LEVEL_MAX);
        self.frames_since_change = 0;
    }

    /// Record a frame duration in milliseconds and return the updated target quality level.
    pub fn observe_frame(&mut self, frame_ms: f32) -> u8 {
        let clamped = frame_ms.clamp(mech::FRM_FRAME_MS_MIN, mech::FRM_FRAME_MS_MAX);
        self.ema_ms = self.ema_ms * (1.0 - mech::FRM_EMA_ALPHA) + clamped * mech::FRM_EMA_ALPHA;
        self.frames_since_change = self.frames_since_change.saturating_add(1);
        if self.frames_since_change >= mech::FRM_HYSTERESIS_FRAMES {
            if self.ema_ms > mech::FRM_DOWNGRADE_MS && self.level > mech::QUALITY_LEVEL_MIN {
                self.level = self
                    .level
                    .saturating_sub(mech::FRM_DOWNGRADE_STEP)
                    .max(mech::QUALITY_LEVEL_MIN);
                self.frames_since_change = 0;
            } else if self.ema_ms < mech::FRM_UPGRADE_MS && self.level < self.max_level {
                self.level = (self.level + mech::FRM_UPGRADE_STEP).min(self.max_level);
                self.frames_since_change = 0;
            }
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;

    fn intel_hd_4000_caps() -> GpuCaps {
        GpuCaps {
            adapter_name: "Intel(R) HD Graphics 4000".into(),
            vendor_id: 0x8086,
            device_id: 0x0166,
            dedicated_vram_mb: 32,
            shared_mem_mb: 1792,
            feature_level: D3D_FEATURE_LEVEL_11_0,
            is_uma: true,
            msaa_fp16_max_samples: 4,
            msaa_rgba8_max_samples: 4,
            max_texture_dim: 16384,
            max_anisotropy: 16,
        }
    }

    fn discrete_gpu_caps(vram_mb: u64, msaa_fp16_max_samples: u32) -> GpuCaps {
        GpuCaps {
            adapter_name: "Test Discrete GPU".into(),
            vendor_id: 0x10DE,
            device_id: 0x2484,
            dedicated_vram_mb: vram_mb,
            shared_mem_mb: 8192,
            feature_level: D3D_FEATURE_LEVEL_11_0,
            is_uma: false,
            msaa_fp16_max_samples,
            msaa_rgba8_max_samples: 4,
            max_texture_dim: 16384,
            max_anisotropy: 16,
        }
    }

    #[test]
    fn intel_hd_4000_uma_caps_in_automatic_and_level21() {
        let caps = intel_hd_4000_caps();
        assert_eq!(QualityProfile::recommended_for_caps(&caps), 5);

        // Automatic quality + FRM: lands on level 5 with UMA-safe caps and no SSAO/DoF/bloom.
        let auto = QualityProfile::resolve(&caps, 0, true, None);
        assert_eq!(auto.level, 5);
        assert!(auto.auto_mode);
        assert_eq!(auto.msaa_samples, 1);
        assert_eq!(auto.shadow_atlas_side, 1024);
        assert_eq!(auto.shadow_cascades, 2);
        assert!(auto.enable_shadows);
        assert!(!auto.enable_ssao);
        assert!(!auto.enable_dof);
        assert!(!auto.enable_bloom);
        assert!(auto.enable_clouds);
        assert_eq!(auto.max_local_shadows, 0);
        assert_eq!(auto.lod_bias, 1.75);
        assert_eq!(auto.max_anisotropy, 1);

        // Even if FRM pushes level up to 21 in auto mode, UMA caps auto ceiling at QUALITY_MID_DEFAULT_LEVEL (12), MSAA=1x, atlas=1024, SSAO/DoF off, aniso<=4.
        let auto_max = QualityProfile::resolve(&caps, 0, true, Some(21));
        assert_eq!(auto_max.level, mech::QUALITY_MID_DEFAULT_LEVEL);
        assert_eq!(auto_max.msaa_samples, 1);
        assert_eq!(auto_max.shadow_atlas_side, 1024);
        assert_eq!(auto_max.max_local_shadows, 4);
        assert_eq!(auto_max.max_anisotropy, 4);
        assert!(!auto_max.enable_ssao);
        assert!(!auto_max.enable_dof);

        // Explicit Level21 (auto_mode = false) enables SSAO/DoF/4 cascades while still capping UMA bandwidth limits.
        let manual = QualityProfile::resolve(&caps, 21, false, None);
        assert_eq!(manual.level, 21);
        assert!(!manual.auto_mode);
        assert_eq!(manual.msaa_samples, 1);
        assert_eq!(manual.shadow_atlas_side, 1024);
        assert_eq!(manual.shadow_cascades, 4);
        assert!(manual.enable_ssao);
        assert!(manual.enable_dof);
        assert!(manual.enable_bloom);
        assert_eq!(manual.max_local_shadows, 4);
        assert_eq!(manual.lod_bias, 1.0);
        assert_eq!(manual.max_anisotropy, 4);
    }

    #[test]
    fn level21_vs_automatic_on_discrete_gpus() {
        let high = discrete_gpu_caps(8192, 4);
        assert_eq!(QualityProfile::recommended_for_caps(&high), 21);

        let p21 = QualityProfile::resolve(&high, 21, false, None);
        assert_eq!(p21.level, 21);
        assert!(!p21.auto_mode);
        assert_eq!(p21.msaa_samples, 4);
        assert_eq!(p21.shadow_atlas_side, 2048);
        assert_eq!(p21.shadow_cascades, 4);
        assert!(p21.enable_shadows);
        assert!(p21.enable_ssao);
        assert!(p21.enable_dof);
        assert!(p21.enable_bloom);
        assert!(p21.enable_clouds);
        assert_eq!(p21.max_local_shadows, 16);
        assert_eq!(p21.lod_bias, 1.0);
        assert_eq!(p21.max_anisotropy, mech::TEXTURE_ANISOTROPY as u32);

        // Mid-VRAM (1 GB) defaults to level 12 in Automatic.
        let mid = discrete_gpu_caps(1024, 4);
        assert_eq!(QualityProfile::recommended_for_caps(&mid), 12);
        let p_mid = QualityProfile::resolve(&mid, 0, true, None);
        assert_eq!(p_mid.level, 12);
        assert_eq!(p_mid.msaa_samples, 1);
        assert_eq!(p_mid.shadow_atlas_side, 1024);
        assert_eq!(p_mid.shadow_cascades, 3);
        assert!(p_mid.enable_ssao);
        assert!(!p_mid.enable_dof);
        assert!(p_mid.enable_bloom);
        assert_eq!(p_mid.max_local_shadows, 4);
        assert_eq!(p_mid.lod_bias, 1.25);
        assert_eq!(p_mid.max_anisotropy, 4);

        // Discrete GPU without 4x FP16 MSAA falls back to level 5 recommended and 1x MSAA even at Level21.
        let no_msaa = discrete_gpu_caps(4096, 1);
        assert_eq!(QualityProfile::recommended_for_caps(&no_msaa), 5);
        assert_eq!(QualityProfile::resolve(&no_msaa, 21, false, None).msaa_samples, 1);

        // Level clamping and explicit FRM override behavior.
        assert_eq!(QualityProfile::from_level(0, &high, false).level, 1);
        assert_eq!(QualityProfile::from_level(99, &high, false).level, 21);
        let frm_active = QualityProfile::resolve(&high, 18, true, Some(9));
        assert!(frm_active.auto_mode);
        assert_eq!(frm_active.level, 9);
        // Manual setting caps FRM rather than allowing FRM to upgrade above it.
        let frm_capped = QualityProfile::resolve(&high, 10, true, Some(21));
        assert_eq!(frm_capped.level, 10);
    }

    #[test]
    fn manual_level01_on_uma_not_upgraded_and_low_tier_disables_heavy_effects() {
        let uma = intel_hd_4000_caps();
        for frm in [None, Some(5), Some(12), Some(21)] {
            let p = QualityProfile::resolve(&uma, 1, true, frm);
            assert_eq!(p.level, 1, "manual Level01 must stay at level 1 for frm={frm:?}");
            assert!(!p.enable_shadows);
            assert!(!p.enable_clouds);
            assert!(!p.enable_bloom);
            assert!(!p.enable_ssao);
            assert!(!p.enable_dof);
            assert_eq!(p.shadow_cascades, 0);
            assert_eq!(p.max_local_shadows, 0);
            assert_eq!(p.max_anisotropy, 1);
        }
        for lvl in 1..=mech::QUALITY_LOW_SHADER_MAX_LEVEL {
            let p = QualityProfile::from_level(lvl, &discrete_gpu_caps(8192, 4), false);
            assert!(!p.enable_shadows);
            assert!(!p.enable_clouds);
            assert!(!p.enable_bloom);
            assert!(!p.enable_ssao);
            assert!(!p.enable_dof);
            assert_eq!(p.shadow_cascades, 0);
            assert_eq!(p.max_local_shadows, 0);
            assert_eq!(p.max_anisotropy, 1);
        }
    }

    #[test]
    fn frame_rate_manager_hysteresis_and_clamping() {
        let high = discrete_gpu_caps(8192, 4);
        let mut frm = FrameRateManager::new(&high);
        assert_eq!(frm.level(), 21);
        assert!((frm.smoothed_ms() - mech::FRM_TARGET_INITIAL_MS).abs() < 1e-4);

        // 1) Deadband stability: 60 frames at 16.6 ms keep level unchanged.
        for _ in 0..60 {
            assert_eq!(frm.observe_frame(16.6), 21);
        }

        // 2) Spike immunity + cooldown hysteresis: 2 slow spikes keep EMA <= 22 ms (no change),
        // frame 3 crosses 22 ms and steps down by 2, resetting the 45-frame cooldown.
        assert_eq!(frm.observe_frame(45.0), 21);
        assert_eq!(frm.observe_frame(45.0), 21);
        assert_eq!(frm.observe_frame(45.0), 19);

        // Immediately after stepping down, cooldown resets so next 44 slow frames stay at 19.
        for _ in 0..(mech::FRM_HYSTERESIS_FRAMES - 1) {
            assert_eq!(frm.observe_frame(45.0), 19);
        }
        assert_eq!(frm.observe_frame(45.0), 17);

        // 3) Upgrade hysteresis: fast frames (4 ms) must wait 45 frames after last change, then step +1.
        frm.set_level(5);
        // Prime EMA down quickly while observing hysteresis wait.
        for _ in 0..(mech::FRM_HYSTERESIS_FRAMES - 1) {
            assert_eq!(frm.observe_frame(4.0), 5);
        }
        assert!(frm.smoothed_ms() < mech::FRM_UPGRADE_MS);
        assert_eq!(frm.observe_frame(4.0), 6);

        // 4) Lower and upper bounds clamping + extreme spike clamping.
        frm.set_level(2);
        for _ in 0..mech::FRM_HYSTERESIS_FRAMES {
            frm.observe_frame(10_000.0);
        }
        assert_eq!(frm.level(), 1);
        assert!(frm.smoothed_ms() <= mech::FRM_FRAME_MS_MAX);
        // Should not underflow below 1 on further slow frames.
        for _ in 0..mech::FRM_HYSTERESIS_FRAMES {
            assert_eq!(frm.observe_frame(10_000.0), 1);
        }

        // Cannot exceed 21 on fast frames.
        frm.set_level(21);
        for _ in 0..mech::FRM_HYSTERESIS_FRAMES {
            assert_eq!(frm.observe_frame(0.0), 21);
        }
        assert!(frm.smoothed_ms() >= mech::FRM_FRAME_MS_MIN);
    }
}
