// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fpower;
use log::{debug, info, warn};

/// Polisher goes through the following stages of battery info processing to obtain better battery
//  level, level status, time to full estimation and so on.
///
/// Data flow:
///                      Raw Data of Battery Level
///                                  |
///                                  V
///                   Initial Scaler: 3-100% => 0-100%
///                                  |
///                                  V
///                               Filters
///                                  |
///                                  |=====> Time to Full
///                                  |
///                                  V
///                        Spoofing and Splicing
///                                  |
///                                  V
///                           rate limiter
///                                  |
///                                  V
///                  Reported to upper level, displayed

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TimeEstimatorError {
    InvalidRange,
    MissingCurrent,
    NonPositiveCurrent,
}

struct InitialScaler;

impl InitialScaler {
    // Scale the initial battery level from 3-100% to 0-100%
    const SHUTDOWN_OFFSET: f32 = 3.0;
    fn scale_level(level: f32) -> f32 {
        if level >= 100.0 {
            return 100.0;
        } else if level >= Self::SHUTDOWN_OFFSET {
            let scaled = (level - Self::SHUTDOWN_OFFSET) * 100.0 / (100.0 - Self::SHUTDOWN_OFFSET);
            return scaled.ceil();
        } else {
            return 0.0;
        }
    }
}

// Size of lookup table from 0% to 99%.
const LOOKUP_TABLE_SIZE: usize = 100;

struct ChargeTimeEstimator {
    baseline_duration_lookup: [i32; LOOKUP_TABLE_SIZE],
}

impl ChargeTimeEstimator {
    // TODO(https://fxbug.dev/442619993): Read all tables from a device tree or a configuration.

    // (duration, threshold) stores number of seconds to gain 1% charge, at level <= corresponding
    // threshold. For 0-78%, the duration = 32 seconds. For 79-86, it's 56 seconds.
    const PERCENT_CHARGE_DURATION: [(i32, u32); 4] = [(32, 78), (56, 86), (84, 96), (92, 100)];

    // Device tree charging current limits translate to this array with 25,000 uA resolution.
    // Temperatures (0C, 10C, 20C, 42C, 46C) map to rows.
    // SOC (0, 84, 90) map to columns.
    const CHG_CC_LIMITS_UA: [[i32; 3]; 4] = [
        [200_000, 100_000, 100_000],
        [275_000, 100_000, 100_000],
        [500_000, 500_000, 200_000],
        [400_000, 400_000, 200_000],
    ];

    // Used to determine the column index for CHG_CC_LIMITS_UA.
    const TTF_TIER_THRESHOLDS: [f32; 3] = [0.0, 84.0, 90.0];

    // Used to determine the row index for CHG_CC_LIMITS_UA.
    // Note: the array is take from an external configuration and the last element is unused.
    const TTF_CHARGE_TEMP_LIMITS: [i32; 5] = [0, 10_000, 20_000, 42_000, 46_000];

    fn get_reference_current_ua(level_percent: f32, temperature_mc: Option<i32>) -> i32 {
        // Default to 25C room temp
        let temp_mc = temperature_mc.unwrap_or(25_000);

        // Find the appropriate temperature row index.
        // Skip the first element containing the absolute minimum (0) when checking upper bounds.
        let row_idx = Self::TTF_CHARGE_TEMP_LIMITS
            .iter()
            .skip(1)
            .position(|&limit| temp_mc < limit)
            .unwrap_or_else(|| Self::CHG_CC_LIMITS_UA.len() - 1);

        // Find the appropriate SOC column index.
        // Skip the first element containing the minimum threshold (0.0) when checking bounds.
        let col_idx = Self::TTF_TIER_THRESHOLDS
            .iter()
            .skip(1)
            .position(|&threshold| level_percent < threshold)
            .unwrap_or_else(|| Self::CHG_CC_LIMITS_UA[0].len() - 1);

        Self::CHG_CC_LIMITS_UA[row_idx][col_idx]
    }

    fn new() -> ChargeTimeEstimator {
        let mut table = [0i32; LOOKUP_TABLE_SIZE];
        let mut percent_start = 0;
        for (duration, threshold) in Self::PERCENT_CHARGE_DURATION.iter() {
            let end = (*threshold).min((LOOKUP_TABLE_SIZE - 1).try_into().unwrap());
            for percent in percent_start..=end {
                table[percent as usize] = *duration;
            }

            percent_start = end + 1;
            if percent_start >= LOOKUP_TABLE_SIZE as u32 {
                break;
            }
        }

        ChargeTimeEstimator { baseline_duration_lookup: table }
    }

    /// Calculates the time to full for the range [from_soc, to_soc].
    fn time_to_full(
        &self,
        from_soc: f32,
        to_soc: f32,
        actual_current_ua: Option<i32>,
        ref_current_ua: i32,
    ) -> Result<zx::BootDuration, TimeEstimatorError> {
        if to_soc > 100.0 || to_soc < from_soc {
            return Err(TimeEstimatorError::InvalidRange);
        }
        if to_soc == from_soc {
            return Ok(zx::Duration::from_seconds(0));
        }

        let ratio = self.ttf_current_ratio(actual_current_ua, ref_current_ua)?;

        // If both from_soc and to_soc fall within the same integer percentage (e.g. 99.2 to 99.8)
        if from_soc.floor() == to_soc.floor() {
            let elap = self.ttf_elap_estimate_step(from_soc.floor() as u32, ratio);
            let estimate_s = elap * (to_soc - from_soc);
            return Ok(zx::Duration::from_seconds(estimate_s as i64));
        }

        let mut estimate_s = 0.0_f32;

        // FIRST: fraction part of from_soc if any
        let from_soc_int = from_soc.floor() as u32;
        let from_soc_frac = from_soc.fract();
        let mut i = from_soc_int;
        if from_soc_frac > 0.0 {
            let elap = self.ttf_elap_estimate_step(i, ratio);
            estimate_s += elap * (1.0 - from_soc_frac);
            i += 1;
        }

        // accumulate ttf_elap_estimate_step starting from i until end
        let last_int = to_soc.floor() as u32;
        while i < last_int {
            let elap = self.ttf_elap_estimate_step(i, ratio);
            estimate_s += elap;
            i += 1;
        }

        // LAST: fraction of to_soc if any
        let to_soc_frac = to_soc.fract();
        if to_soc_frac > 0.0 {
            let elap = self.ttf_elap_estimate_step(last_int, ratio);
            estimate_s += elap * to_soc_frac;
        }

        Ok(zx::Duration::from_seconds(estimate_s as i64))
    }

    // Predict the time in seconds needed to charge by 1% according to the lookup table.
    fn get_level_duration(&self, level: u32) -> i32 {
        let level = level as usize;
        if level >= LOOKUP_TABLE_SIZE {
            return 0;
        }
        self.baseline_duration_lookup[level]
    }

    /// Calculates the power ratio used to scale time-to-full estimations.
    ///
    /// # Errors
    /// Returns `MissingCurrent` if `actual_current_ua` is `None`.
    /// Returns `NonPositiveCurrent` if `actual_current_ua <= 0`.
    fn ttf_current_ratio(
        &self,
        actual_current_ua: Option<i32>,
        ref_current_ua: i32,
    ) -> Result<f32, TimeEstimatorError> {
        let actual_current = actual_current_ua.ok_or(TimeEstimatorError::MissingCurrent)?;

        if actual_current <= 0 {
            return Err(TimeEstimatorError::NonPositiveCurrent);
        }

        // Scale the reference ideally but clamp the ratio at 1.0.
        if actual_current < ref_current_ua {
            Ok(ref_current_ua as f32 / actual_current as f32)
        } else {
            Ok(1.0)
        }
    }

    /// Calculates the elapsed time to charge a single 1% SOC step.
    fn ttf_elap_estimate_step(&self, level: u32, ratio: f32) -> f32 {
        let base_elap = self.get_level_duration(level) as f32;
        base_elap * ratio
    }
}

// Determine the LevelStatus
struct LevelChecker;

impl LevelChecker {
    // Used to determine the level_status, after scale_level
    const THRESHOLD_LEVEL_OK: f32 = 80.0;
    const THRESHOLD_LEVEL_WARNING: f32 = 30.0;
    const THRESHOLD_LEVEL_LOW: f32 = 0.0;

    fn determine_level_status(
        level: f32,
        charger_status: Option<fpower::ChargeStatus>,
    ) -> fpower::LevelStatus {
        if level > Self::THRESHOLD_LEVEL_OK {
            return fpower::LevelStatus::Ok;
        } else if level > Self::THRESHOLD_LEVEL_WARNING {
            return fpower::LevelStatus::Warning;
        } else if level > Self::THRESHOLD_LEVEL_LOW {
            return fpower::LevelStatus::Low;
        } else if charger_status == Some(fpower::ChargeStatus::NotCharging)
            || charger_status == Some(fpower::ChargeStatus::Discharging)
        {
            return fpower::LevelStatus::Critical;
        } else if charger_status == Some(fpower::ChargeStatus::Unknown) || charger_status == None {
            return fpower::LevelStatus::Unknown;
        } else {
            return fpower::LevelStatus::Low;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CurvePoint {
    real: f32,
    ui: f32,
}

/// Tracks which curve is currently active.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CurveState {
    Unmodified,
    Spoofing,
    Splicing(CurvePoint),
}

struct CurveMapper {
    curve_state: CurveState,
    prev_ui_level: f32,
    prev_charge_status: Option<fpower::ChargeStatus>,
}

impl CurveMapper {
    // Constants for battery level spoofing to report 100% before reaching there.
    // TODO(https://fxbug.dev/422755268): Make these constants configurable.
    const LEVEL_TRUE: f32 = 15.0;
    const LEVEL_SPOOF: f32 = 95.0;
    const LEVEL_FULL: f32 = 100.0;

    pub fn new() -> CurveMapper {
        CurveMapper {
            curve_state: CurveState::Unmodified,
            prev_ui_level: 0.0,
            prev_charge_status: None,
        }
    }

    fn splice_for_level(level: f32, left_point: CurvePoint, right_point: CurvePoint) -> f32 {
        if level < left_point.real {
            level
        } else if level < right_point.real {
            // Interpolate between left_point and right_point
            left_point.ui
                + (level - left_point.real) * (right_point.ui - left_point.ui)
                    / (right_point.real - left_point.real)
        } else {
            right_point.ui
        }
    }

    /// Calculates the UI level using the discharging splicing curve.
    fn splice_for_discharging(real_level: f32, mid_point: CurvePoint) -> f32 {
        debug!("mid point for discharging: {:?}", mid_point);
        Self::splice_for_level(
            real_level,
            CurvePoint { real: Self::LEVEL_TRUE, ui: Self::LEVEL_TRUE },
            mid_point,
        )
    }

    /// Calculates the UI level using the charging splicing curve.
    fn splice_for_charging(real_level: f32, mid_point: CurvePoint) -> f32 {
        debug!("mid point for charging: {:?}", mid_point);
        Self::splice_for_level(
            real_level,
            mid_point,
            CurvePoint { real: Self::LEVEL_FULL, ui: Self::LEVEL_FULL },
        )
    }

    // Applies the different fitting logic according to state transition:
    // 1. When first started, always in Unmodified state (TRUE);
    //    Only leaves Unmodified state and reach Spoofing when level is Full;
    // 2. From Spoofing, can only arrive at Splicing when level drops below 95%;
    // 3. From Splicing, can reach Unmodified at 15%, or Spoofing at Full;
    //    Within Splicing, if charging direction changes, record the mid point.
    fn determine_new_state(&mut self, level: f32, charge_status: Option<fpower::ChargeStatus>) {
        let new_curve_state = match self.curve_state {
            CurveState::Unmodified => {
                if charge_status == Some(fpower::ChargeStatus::Full) {
                    CurveState::Spoofing
                } else {
                    self.curve_state
                }
            }
            CurveState::Spoofing => {
                if level < Self::LEVEL_SPOOF {
                    CurveState::Splicing(CurvePoint {
                        real: Self::LEVEL_SPOOF,
                        ui: Self::LEVEL_FULL,
                    })
                } else {
                    self.curve_state
                }
            }
            CurveState::Splicing(_mid_point_ref) => {
                if level < Self::LEVEL_TRUE {
                    CurveState::Unmodified
                } else if charge_status == Some(fpower::ChargeStatus::Full)
                    && level > Self::LEVEL_SPOOF
                {
                    CurveState::Spoofing
                } else if self.prev_charge_status != charge_status {
                    // Assuming charge status direction changes without level changes
                    CurveState::Splicing(CurvePoint { real: level, ui: self.prev_ui_level })
                } else {
                    self.curve_state
                }
            }
        };
        if new_curve_state != self.curve_state {
            info!("curve_state changed from {:?} to {:?}", self.curve_state, new_curve_state);
        }
        self.curve_state = new_curve_state;
    }

    fn adjust_level(&mut self, level: f32, info: &mut fpower::BatteryInfo) {
        let new_level = match self.curve_state {
            CurveState::Spoofing => Self::LEVEL_FULL,
            CurveState::Splicing(mid_point) => {
                if info.charge_status == Some(fpower::ChargeStatus::Charging) {
                    Self::splice_for_charging(level, mid_point)
                } else {
                    Self::splice_for_discharging(level, mid_point)
                }
            }
            _ => level,
        };

        self.prev_ui_level = level;
        self.prev_charge_status = info.charge_status;

        info.level_percent = Some(new_level);
    }
}

type TimeStampNs = zx::sys::zx_time_t;
type TimeDeltaSecs = f32;

struct RateLimiter {
    max_rate: f32,
    rl_ssoc_target: f32,
    rl_ssoc_last_update: TimeStampNs,
    rl_current_level: f32,
    is_initialized: bool,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RateLimiter::RL_MAX_DELTA_SOC / RateLimiter::RL_MAX_TIME_S)
    }
}

impl RateLimiter {
    // TODO(https://fxbug.dev/442619993): Read this table from a device tree or a configuration.
    const RL_MAX_DELTA_SOC: f32 = 2.0;
    const RL_MAX_TIME_S: f32 = 15.0;
    const NANO_SECOND_TO_SECONDS: f32 = 0.000000001;

    fn new(rate: f32) -> RateLimiter {
        RateLimiter {
            max_rate: rate,
            rl_ssoc_target: 0.0,
            rl_ssoc_last_update: 0,
            rl_current_level: 0.0,
            is_initialized: false,
        }
    }

    fn apply_rate_limit(
        &mut self,
        current_target: f32,
        is_charging: bool,
        current_timestamp_ns: TimeStampNs,
    ) -> f32 {
        // POWER-ON
        if !self.is_initialized {
            self.rl_ssoc_target = current_target;
            self.rl_current_level = current_target;
            self.rl_ssoc_last_update = current_timestamp_ns;
            self.is_initialized = true;
            return self.rl_current_level;
        }

        let now = current_timestamp_ns;
        let delta_time_s: TimeDeltaSecs =
            (now - self.rl_ssoc_last_update) as f32 * Self::NANO_SECOND_TO_SECONDS;
        if delta_time_s <= 0.0 {
            // If no time passed, return current level and don't update last time
            return self.rl_current_level;
        }
        let max_delta = self.max_rate * delta_time_s;

        // limit according to charging status
        let new_target =
            if is_charging { current_target } else { current_target.min(self.rl_ssoc_target) };
        self.rl_ssoc_target = new_target.clamp(0.0, 100.0);

        // Calculate step to target
        let limiting;
        let mut step = self.rl_ssoc_target - self.rl_current_level;
        if step.abs() > max_delta {
            step = if step > 0.0 { max_delta } else { -max_delta };
            limiting = true;
        } else {
            limiting = false;
        }

        let new_level = self.rl_current_level + step;
        self.rl_ssoc_last_update = now;
        self.rl_current_level = new_level.clamp(0.0, 100.0);

        if limiting {
            info!(
                "RateLimit: Target={:.2}, MaxDelta={:.4}, Step={:.4}, NewLevel={:.2} Timestamp={:?}",
                self.rl_ssoc_target, max_delta, step, self.rl_current_level, now,
            );
        }

        self.rl_current_level
    }

    fn reset(&mut self) {
        self.is_initialized = false;
    }
}

pub(crate) struct Polisher {
    curve_mapper: CurveMapper,
    last_level: Option<f32>,
    last_post_curve: Option<f32>,
    estimator: ChargeTimeEstimator,
    rate_limiter: RateLimiter,
}

impl Polisher {
    pub fn new() -> Polisher {
        Polisher {
            curve_mapper: CurveMapper::new(),
            last_level: None,
            last_post_curve: None,
            estimator: ChargeTimeEstimator::new(),
            rate_limiter: RateLimiter::default(),
        }
    }

    fn scale_battery_level(&self, info: &mut fpower::BatteryInfo) {
        if let Some(level) = info.level_percent {
            info.level_percent = Some(InitialScaler::scale_level(level as f32));
        }
    }

    fn set_level_status(&self, info: &mut fpower::BatteryInfo) {
        if let Some(level) = info.level_percent {
            info.level_status =
                Some(LevelChecker::determine_level_status(level as f32, info.charge_status));
        }
    }

    fn process_curve_state(&mut self, info: &mut fpower::BatteryInfo) {
        let Some(level) = info.level_percent else { return };
        self.curve_mapper.determine_new_state(level, info.charge_status);
        self.curve_mapper.adjust_level(level, info);
    }

    fn calculate_time_to_full(&self, info: &mut fpower::BatteryInfo) {
        let Some(level) = info.level_percent else {
            warn!("level shouldn't be none");
            info.time_remaining = Some(fpower::TimeRemaining::Indeterminate(0));
            return;
        };

        let ref_current = ChargeTimeEstimator::get_reference_current_ua(level, info.temperature_mc);

        let actual_current = info.average_charging_current_ua.or(info.present_charging_current_ua);
        if info.charge_status != Some(fpower::ChargeStatus::Charging) {
            info.time_remaining = Some(fpower::TimeRemaining::Indeterminate(0));
            return;
        }

        let time_to_full_estimate =
            match self.estimator.time_to_full(level, 100.0, actual_current, ref_current) {
                Ok(duration) => duration.into_nanos(),
                Err(e) => {
                    warn!("Failed to estimate time to full: {:?}", e);
                    info.time_remaining = Some(fpower::TimeRemaining::Indeterminate(0));
                    return;
                }
            };
        info.time_remaining = Some(fpower::TimeRemaining::FullCharge(time_to_full_estimate));
    }

    fn rate_limit_level(&mut self, info: &mut fpower::BatteryInfo) {
        let Some(level) = info.level_percent else {
            warn!("Missing level for rate limiter");
            return;
        };
        let Some(timestamp_ns) = info.timestamp else {
            warn!("Missing timestamp for rate limiter");
            return;
        };
        let is_charging_or_full = info.charge_status == Some(fpower::ChargeStatus::Charging)
            || info.charge_status == Some(fpower::ChargeStatus::Full);

        // The curve-mapped level becomes the *target* for the rate limiter.
        let rate_limited_level =
            self.rate_limiter.apply_rate_limit(level, is_charging_or_full, timestamp_ns);

        info.level_percent = Some(rate_limited_level);
    }

    pub fn polish_info(&mut self, info: fpower::BatteryInfo) -> fpower::BatteryInfo {
        let original_level = info.level_percent;
        let mut info = info;
        self.scale_battery_level(&mut info);
        self.set_level_status(&mut info);
        let scaled_level = info.level_percent;
        self.calculate_time_to_full(&mut info);
        self.process_curve_state(&mut info);
        let post_curve = info.level_percent;
        self.rate_limit_level(&mut info);

        if self.last_level != original_level || self.last_post_curve != post_curve {
            info!(
                "Levels - original: {:?}, scaled: {:?}, post curve mapping: {:?}, rate limited: {:?}",
                original_level, scaled_level, post_curve, info.level_percent
            );
            self.last_level = original_level;
            self.last_post_curve = post_curve;
        }
        info
    }

    pub fn reset_rate_limiter(&mut self) {
        self.rate_limiter.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[fuchsia::test]
    fn test_scale_level() {
        assert_eq!(InitialScaler::scale_level(InitialScaler::SHUTDOWN_OFFSET), 0.0);
        assert_eq!(InitialScaler::scale_level(13.0), 11.0);
        assert_eq!(InitialScaler::scale_level(51.0), 50.0);
        assert_eq!(InitialScaler::scale_level(51.5), 50.0);
        assert_eq!(InitialScaler::scale_level(99.0), 99.0);
        assert_eq!(InitialScaler::scale_level(100.0), 100.0);
        assert_eq!(InitialScaler::scale_level(101.0), 100.0);
    }

    #[fuchsia::test]
    fn test_determine_level_status() {
        assert_eq!(
            LevelChecker::determine_level_status(0.0, Some(fpower::ChargeStatus::NotCharging)),
            fpower::LevelStatus::Critical
        );
        assert_eq!(
            LevelChecker::determine_level_status(0.0, Some(fpower::ChargeStatus::Discharging)),
            fpower::LevelStatus::Critical
        );
        assert_eq!(
            LevelChecker::determine_level_status(0.0, Some(fpower::ChargeStatus::Unknown)),
            fpower::LevelStatus::Unknown
        );
        assert_eq!(LevelChecker::determine_level_status(0.0, None), fpower::LevelStatus::Unknown);
        assert_eq!(
            LevelChecker::determine_level_status(LevelChecker::THRESHOLD_LEVEL_OK + 1.0, None),
            fpower::LevelStatus::Ok
        );
        assert_eq!(
            LevelChecker::determine_level_status(LevelChecker::THRESHOLD_LEVEL_WARNING + 1.0, None),
            fpower::LevelStatus::Warning
        );
        assert_eq!(
            LevelChecker::determine_level_status(LevelChecker::THRESHOLD_LEVEL_LOW + 1.0, None),
            fpower::LevelStatus::Low
        );
        assert_eq!(
            LevelChecker::determine_level_status(100.0, Some(fpower::ChargeStatus::Charging)),
            fpower::LevelStatus::Ok
        );
    }

    #[fuchsia::test]
    fn test_scale_battery_level() {
        let polisher = Polisher::new();

        // Test when level_percent = shutdown offset
        let mut info = fpower::BatteryInfo {
            level_percent: Some(InitialScaler::SHUTDOWN_OFFSET),
            ..Default::default()
        };
        polisher.scale_battery_level(&mut info);
        assert_eq!(info.level_percent, Some(0.0));

        // Test when level_percent = 100%
        info.level_percent = Some(100.0);
        polisher.scale_battery_level(&mut info);
        assert_eq!(info.level_percent, Some(100.0));
    }

    // Helper to create a default BatteryInfo for tests
    fn new_info(level: f32, status: fpower::ChargeStatus) -> fpower::BatteryInfo {
        fpower::BatteryInfo {
            level_percent: Some(level),
            charge_status: Some(status),
            ..Default::default()
        }
    }

    #[fuchsia::test]
    fn test_normal_charging_is_one_to_one() {
        let mut polisher = Polisher::new();
        // Input a normal charging level
        let info = polisher.polish_info(new_info(83.0, fpower::ChargeStatus::Charging));

        // The spoof function should see it's a normal charge and do nothing.
        // The final level should just be the scaled value.
        let expected_level = InitialScaler::scale_level(83.0);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Unmodified);
    }

    #[fuchsia::test]
    fn test_unplug_unmodified_while_charging_is_smooth() {
        let mut polisher = Polisher::new();
        // Establish that we are in a charging state.
        let _ = polisher.polish_info(new_info(95.0, fpower::ChargeStatus::Charging));
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Unmodified);

        // Unplug at 96%.
        let _ = polisher.polish_info(new_info(96.0, fpower::ChargeStatus::Charging));
        let info = polisher.polish_info(new_info(96.0, fpower::ChargeStatus::Discharging));
        let expected_level = InitialScaler::scale_level(96.0);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Unmodified);
    }

    // Helper to calculate the expected smooth level (from previous analysis)
    fn calculate_expected_splice_level(raw_level: f32) -> f32 {
        let left = CurvePoint { real: CurveMapper::LEVEL_TRUE, ui: CurveMapper::LEVEL_TRUE };
        let right = CurvePoint { real: CurveMapper::LEVEL_SPOOF, ui: CurveMapper::LEVEL_FULL };

        CurveMapper::splice_for_level(raw_level, left, right)
    }

    #[fuchsia::test]
    fn test_drain_while_full() {
        let mut polisher = Polisher::new();
        const T_SEC: i64 = 1000;
        let mut timestamp: zx::sys::zx_time_t = 0;

        // --- SETUP: CHARGE TO 100% AND ENTER SPOOFING ---
        let mut info = new_info(98.0, fpower::ChargeStatus::Charging);
        info.timestamp = Some(timestamp);
        let _ = polisher.polish_info(info);

        timestamp += T_SEC * NANOS_PER_SEC;
        let mut info = new_info(100.0, fpower::ChargeStatus::Full);
        info.timestamp = Some(timestamp);
        let polished = polisher.polish_info(info);

        // Check Spoofing is active and UI is 100.0
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Spoofing);
        assert_eq!(polished.level_percent, Some(100.0), "Full level should be 100.0%");

        timestamp += T_SEC * NANOS_PER_SEC;
        let level_drops_to = 97.0;
        let mut info = new_info(level_drops_to, fpower::ChargeStatus::Full);
        info.timestamp = Some(timestamp);
        let polished = polisher.polish_info(info);
        assert_eq!(polished.level_percent, Some(100.0), "Spoofed level should be 100.0%");

        // Just drops out of spoofing while on charger.
        timestamp += T_SEC * NANOS_PER_SEC;
        let level_drops_to = 94.0;
        let mut info = new_info(level_drops_to, fpower::ChargeStatus::Full);
        info.timestamp = Some(timestamp);
        let polished = polisher.polish_info(info);

        let scaled_level = InitialScaler::scale_level(level_drops_to);
        let expected_level = calculate_expected_splice_level(scaled_level);
        assert_ne!(polished.level_percent, Some(level_drops_to), "Spliced level should change");
        assert_eq!(polished.level_percent, Some(expected_level));

        // Check not zig zag
        timestamp += T_SEC * NANOS_PER_SEC;
        let level_drops_to = 93.0;
        let mut info = new_info(level_drops_to, fpower::ChargeStatus::Full);
        info.timestamp = Some(timestamp);
        let polished = polisher.polish_info(info);

        let scaled_level = InitialScaler::scale_level(level_drops_to);
        let expected_level = calculate_expected_splice_level(scaled_level);
        assert_ne!(polished.level_percent, Some(100.0), "Spliced level should change");
        assert_eq!(polished.level_percent, Some(expected_level));
    }

    #[fuchsia::test]
    fn test_comprehensive_cycle_without_zig_zag() {
        let mut polisher = Polisher::new();
        const T_SEC: i64 = 1000;
        let mut timestamp: zx::sys::zx_time_t = 0;

        // --- SETUP: Start with 10%  ---
        let mut info = new_info(10.0, fpower::ChargeStatus::Charging);
        info.timestamp = Some(timestamp);
        let polished = polisher.polish_info(info);
        assert!(polished.level_percent.is_some());
        let mut previous_level = polished.level_percent.unwrap();

        // --- CHARGE TO 90% ---
        for i in 11..=90 {
            timestamp += T_SEC * NANOS_PER_SEC;
            let mut info = new_info(i as f32, fpower::ChargeStatus::Charging);
            info.timestamp = Some(timestamp);
            let polished = polisher.polish_info(info);

            info!("Looping charging with input {}", i);
            assert!(polished.level_percent.is_some());
            let current_level = polished.level_percent.unwrap();
            assert!(
                current_level > previous_level,
                "level_percent should increase during charging"
            );
            previous_level = current_level;
        }

        // --- DISCHARGING TO 50% ---
        for i in (50..89).rev() {
            timestamp += T_SEC * NANOS_PER_SEC;
            let mut info = new_info(i as f32, fpower::ChargeStatus::Discharging);
            info.timestamp = Some(timestamp);
            let polished = polisher.polish_info(info);

            info!("Looping discharging with input {}", i);
            assert!(polished.level_percent.is_some());
            let current_level = polished.level_percent.unwrap();
            assert!(
                current_level < previous_level,
                "level_percent should decrease during charging"
            );
            previous_level = current_level;
        }

        // --- CHARGE TO 90% ---
        for i in 51..=100 {
            timestamp += T_SEC * NANOS_PER_SEC;
            let mut info = new_info(i as f32, fpower::ChargeStatus::Charging);
            info.timestamp = Some(timestamp);
            let polished = polisher.polish_info(info);

            info!("Looping charging with input {}", i);
            assert!(polished.level_percent.is_some());
            let current_level = polished.level_percent.unwrap();
            assert!(
                current_level > previous_level,
                "level_percent should increase during charging"
            );
            previous_level = current_level;
        }

        // --- DISCHARGING TO 70% but FULL ---
        for i in (70..100).rev() {
            timestamp += T_SEC * NANOS_PER_SEC;
            let mut info = new_info(i as f32, fpower::ChargeStatus::Full);
            info.timestamp = Some(timestamp);
            let polished = polisher.polish_info(info);

            info!("Looping discharging with input {}", i);
            assert!(polished.level_percent.is_some());
            let current_level = polished.level_percent.unwrap();
            assert!(
                current_level <= previous_level,
                "level_percent should not increase when draining while full"
            );
            previous_level = current_level;
        }
    }

    #[fuchsia::test]
    fn test_get_level_duration_lookup() {
        let estimator = ChargeTimeEstimator::new();
        // 1. Below the lowest threshold (78)
        assert_eq!(estimator.get_level_duration(70), 32, "Level 70 should return 32.");
        assert_eq!(estimator.get_level_duration(78), 32, "Level 78 should return 32.");

        // 2. Between the first two thresholds (78 < L <= 86)
        assert_eq!(estimator.get_level_duration(79), 56, "Level 79 should return 56.");
        assert_eq!(estimator.get_level_duration(85), 56, "Level 85 should return 56.");
        assert_eq!(estimator.get_level_duration(86), 56, "Level 86 should return 56.");

        // 3. Between 86 and 96
        assert_eq!(estimator.get_level_duration(95), 84, "Level 95 should return 84.");
        assert_eq!(estimator.get_level_duration(96), 84, "Level 96 should return 84.");

        // 4. Near full (96 < L <= 100)
        assert_eq!(estimator.get_level_duration(97), 92, "Level 97 should return 92.");
        assert_eq!(estimator.get_level_duration(99), 92, "Level 99 should return 92.");
        assert_eq!(estimator.get_level_duration(100), 0, "Level 100 should return 0.");

        // 5. Above table maximum (u32 input)
        assert_eq!(estimator.get_level_duration(101), 0, "Level 101 should return 0.");
    }

    #[fuchsia::test]
    fn test_get_reference_current_ua() {
        // Temperature below 10C (row 0), SOC < 84 (col 0)
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(50.0, Some(5_000)), 200_000);

        // Temperature 10-20C (row 1), SOC < 84 (col 0)
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(50.0, Some(15_000)), 275_000);

        // Temperature 20-42C (row 2), SOC 84-90 (col 1)
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(85.0, Some(25_000)), 500_000);

        // Temperature 42-46C (row 3, SOC 84-90 (col 1))
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(89.0, Some(45_000)), 400_000);

        // Temperature > 46C (row 3, saturating fallback)
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(95.0, Some(50_000)), 200_000);

        // Default temperature (25_000)
        assert_eq!(ChargeTimeEstimator::get_reference_current_ua(50.0, None), 500_000);
    }

    const NANOS_PER_SEC: i64 = 1_000_000_000;

    #[fuchsia::test]
    fn test_time_to_full() {
        let estimator = ChargeTimeEstimator::new();

        // Pre-calculated Bucket Sums (Seconds):
        // 79-86 (56s/level) = 8 * 56 = 448
        // 87-96 (84s/level) = 10 * 84 = 840
        // 97-100 (92s/level) = 4 * 92 = 368
        // 100 (0s/level) = 0
        // Total seconds from 78 to 100: 448 + 840 + 368 = 1656

        // --- CASE 1: Full (100.0) ---
        assert_eq!(
            estimator
                .time_to_full(
                    100.0,
                    100.0,
                    // If None, it directly resolves to 0 on loop check before hitting pwr_ratio
                    None,
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            0
        );

        // --- CASE 2: Near Full (99.0) ---
        // Sums: 99, 100 (2 levels) -> Call(99)=92s, Call(100)=0s. Total: 92s.
        let expected_99 = 92;
        assert_eq!(
            estimator
                .time_to_full(
                    99.0,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            expected_99
        );

        // --- CASE 3: Level 91.0 (Starts sum at 91) ---
        // Levels 91-96 (6 * 84s) + 97-99 (3 * 92s) + 100 (0s) = 504 + 276 = 780 seconds
        let expected_91 = 780;
        assert_eq!(
            estimator
                .time_to_full(
                    91.0,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            expected_91
        );

        // --- CASE 4: Level 85.0 (Starts sum at 85)
        // Levels 85-86 (2 * 56s) + Levels 87-100 (10*84 + 3*92 + 0)
        // Sums: (2 * 56) + 840 + 276 = 112 + 1116 = 1228 seconds
        let expected_85 = 1228;
        assert_eq!(
            estimator
                .time_to_full(
                    85.0,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            expected_85,
            "At 85%, time remaining should be 1228 seconds."
        );

        // --- CASE 5: Level 50.0 (Starts sum at 50)
        // Levels 50-78 (29 * 32s) + Levels 79-100 (448 + 840 + 276)
        // Sums: (29 * 32) + 1564 = 928 + 1564 = 2492 seconds
        let expected_50 = 2492;
        assert_eq!(
            estimator
                .time_to_full(
                    50.0,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            expected_50,
            "At 50%, time remaining should be 2492 seconds."
        );

        // --- CASE 6: Level 0.0% ---
        // Total seconds: (4184 - 92) = 4092 seconds.
        let expected_0 = 4092;
        assert_eq!(
            estimator
                .time_to_full(
                    0.0,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            expected_0,
            "At 0%, time remaining should be 4092 seconds."
        );
    }

    #[fuchsia::test]
    fn test_charge_time_estimator_fractional() {
        let estimator = ChargeTimeEstimator::new();

        // 99.5% should be half of 99s level duration (92 / 2 = 46s)
        assert_eq!(
            estimator
                .time_to_full(
                    99.5,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            46
        );

        // 98.2% = 0.8 * 92s (for 98 -> 99) + 92s (for 99 -> 100)
        // 0.8 * 92 = 73.6 (rounded to 73 in integer math: 92 * 80 / 100 = 73)
        // Test fractional calculation: 98.2% to 100.0%
        assert_eq!(
            estimator
                .time_to_full(
                    98.2,
                    100.0,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            165
        );

        // Test intra-level fractional calculation: 99.2% to 99.8%
        // elapsed for level 99 is 92. Since diff is 0.6, it should be 92 * 0.6 = 55.2 -> 55
        assert_eq!(
            estimator
                .time_to_full(
                    99.2,
                    99.8,
                    Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            55
        );

        // Test out of bounds SOC/last ratio
        assert_eq!(
            estimator.time_to_full(
                100.0,
                90.0, // Error: last < soc
                Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None)),
                ChargeTimeEstimator::get_reference_current_ua(100.0, None)
            ),
            Err(TimeEstimatorError::InvalidRange)
        );

        // Test missing actual_current (None) yields MissingCurrent properly
        assert_eq!(
            estimator.time_to_full(
                50.0,
                100.0,
                None, // Missing actual battery current
                ChargeTimeEstimator::get_reference_current_ua(100.0, None)
            ),
            Err(TimeEstimatorError::MissingCurrent)
        );
    }

    #[fuchsia::test]
    fn test_charge_time_estimator_ratio() {
        let estimator = ChargeTimeEstimator::new();
        let ref_current = ChargeTimeEstimator::get_reference_current_ua(100.0, None);

        // Base case with ref current: should match None
        assert_eq!(
            estimator
                .time_to_full(
                    99.0,
                    100.0,
                    Some(ref_current),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            92
        );

        // Half current -> double time: 92 * (100 / 0.5) / 100 = 184
        assert_eq!(
            estimator
                .time_to_full(
                    99.0,
                    100.0,
                    Some(ref_current / 2),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            184
        );

        // Negative current -> returns Err instead of falling back to base case
        assert_eq!(
            estimator.time_to_full(
                99.0,
                100.0,
                Some(-100),
                ChargeTimeEstimator::get_reference_current_ua(100.0, None)
            ),
            Err(TimeEstimatorError::NonPositiveCurrent)
        );

        // Zero current -> returns Err instead of falling back to base case
        assert_eq!(
            estimator.time_to_full(
                99.0,
                100.0,
                Some(0),
                ChargeTimeEstimator::get_reference_current_ua(100.0, None)
            ),
            Err(TimeEstimatorError::NonPositiveCurrent)
        );

        // Very high current -> capped at ref current (ratio 100)
        assert_eq!(
            estimator
                .time_to_full(
                    99.0,
                    100.0,
                    Some(ref_current * 2),
                    ChargeTimeEstimator::get_reference_current_ua(100.0, None)
                )
                .unwrap()
                .into_seconds(),
            92
        );
    }

    #[fuchsia::test]
    fn test_calculate_time_to_full() {
        let polisher = Polisher::new();

        // Test None
        let mut info = fpower::BatteryInfo {
            charge_status: Some(fpower::ChargeStatus::Charging),
            ..Default::default()
        };
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(info.time_remaining, Some(fpower::TimeRemaining::Indeterminate(0)),);

        // Test glitched negative current
        info = new_info(50.0, fpower::ChargeStatus::Charging);
        info.average_charging_current_ua = Some(-1);
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(info.time_remaining, Some(fpower::TimeRemaining::Indeterminate(0)));

        // Test 50%
        let expected_50 = 2492 * NANOS_PER_SEC;
        info = new_info(50.0, fpower::ChargeStatus::Charging);
        info.average_charging_current_ua =
            Some(ChargeTimeEstimator::get_reference_current_ua(50.0, None));
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(expected_50)),
            "At 100%, time remaining should be 0 seconds."
        );

        // Test 100%
        info = new_info(100.0, fpower::ChargeStatus::Charging);
        info.average_charging_current_ua =
            Some(ChargeTimeEstimator::get_reference_current_ua(100.0, None));
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(0)),
            "At 100%, time remaining should be 0 seconds."
        );
    }

    #[fuchsia::test]
    fn test_calculate_time_to_full_with_current_limits() {
        let polisher = Polisher::new();

        // Level 50%, temperature 25C (25000 mc)
        // From get_reference_current_ua:
        // temp_mc = 25000 -> row_idx = 2 (20-42C)
        // level = 50.0 -> col_idx = 0 (< 84%)
        // CHG_CC_LIMITS_UA[2][0] = 500_000 uA
        let ref_current_ua = 500_000;

        // Actual current is half of ref current (ratio = 50)
        let actual_current_ua = ref_current_ua / 2;

        let mut info = new_info(50.0, fpower::ChargeStatus::Charging);
        info.temperature_mc = Some(25_000);
        info.average_charging_current_ua = Some(actual_current_ua);

        polisher.calculate_time_to_full(&mut info);

        // Base time for 50% -> 100% is 2492 seconds
        // Ratio = 100 / 0.5 = 200 (since actual is half ref)
        // Time = 2492 * 200 / 100 = 4984 seconds
        let expected_seconds = 4984;
        let expected_nanos = expected_seconds * NANOS_PER_SEC;

        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(expected_nanos)),
            "At 50% with half the reference current, time remaining should be double the base time."
        );
    }

    #[fuchsia::test]
    fn test_splice_for_level() {
        let left = CurvePoint { real: 0.0, ui: 0.0 };
        let right = CurvePoint { real: 100.0, ui: 100.0 };
        let spliced_level = CurveMapper::splice_for_level(25.0, left, right);
        assert_eq!(spliced_level, 25.0);

        let left = CurvePoint { real: 10.0, ui: 0.0 };
        let right = CurvePoint { real: 90.0, ui: 100.0 };
        let spliced_level = CurveMapper::splice_for_level(30.0, left, right);
        assert_eq!(spliced_level, 25.0);
        let spliced_level = CurveMapper::splice_for_level(70.0, left, right);
        assert_eq!(spliced_level, 75.0);
    }

    fn seconds_to_nanoseconds(sec: TimeStampNs) -> TimeStampNs {
        sec * NANOS_PER_SEC
    }

    #[fuchsia::test]
    fn test_rate_limiter_advances() {
        // Pick a better rate: 2% / 16 seconds
        let mut rl = RateLimiter::new(0.125);

        let t0: TimeStampNs = seconds_to_nanoseconds(100);
        let initial_level: f32 = 50.0;
        rl.apply_rate_limit(initial_level, true, t0);

        // Advance time by 24 seconds with a huge jump to 100.
        let t1: TimeStampNs = seconds_to_nanoseconds(124);
        let target_level: f32 = 100.0;

        // Max change allowed: (2.0 / 16.0) * 24.0 = 3.0%
        let result = rl.apply_rate_limit(target_level, true, t1);

        // The level should move by the Max Allowed Delta: 50.0 + 3.0
        assert_eq!(result, 53.0, "Level should advance by 3.0% in 24 seconds.");
        assert_eq!(
            rl.rl_ssoc_last_update,
            seconds_to_nanoseconds(124),
            "Last update time should be 124."
        );
        assert_eq!(rl.rl_current_level, 53.0);

        // Advance time by 32 seconds with a small jump by 1%
        let t2: TimeStampNs = seconds_to_nanoseconds(156);
        let target_level: f32 = 54.0;

        let result = rl.apply_rate_limit(target_level, true, t2);

        assert_eq!(result, 54.0, "Level should advance by 1.0% in 32 seconds.");
        assert_eq!(
            rl.rl_ssoc_last_update,
            seconds_to_nanoseconds(156),
            "Last update time should be 156."
        );
        assert_eq!(rl.rl_current_level, 54.0);

        // Advance time by 16 seconds with a small drop by 1%
        let t3: TimeStampNs = seconds_to_nanoseconds(172);
        let target_level: f32 = 53.0;

        let result = rl.apply_rate_limit(target_level, false, t3);

        assert_eq!(result, 53.0, "Level should advance by -1.0% in 16 seconds.");
        assert_eq!(
            rl.rl_ssoc_last_update,
            seconds_to_nanoseconds(172),
            "Last update time should be 172."
        );
        assert_eq!(rl.rl_current_level, 53.0);

        // Advance time by 16 seconds with a large drop by 10%
        let t3: TimeStampNs = seconds_to_nanoseconds(188);
        let target_level: f32 = 43.0;

        let result = rl.apply_rate_limit(target_level, false, t3);

        assert_eq!(result, 51.0, "Level should advance by -2.0% in 16 seconds.");
        assert_eq!(
            rl.rl_ssoc_last_update,
            seconds_to_nanoseconds(188),
            "Last update time should be 188."
        );
        assert_eq!(rl.rl_current_level, 51.0);

        // Advance time by 16 seconds with a fluctuation.
        let t3: TimeStampNs = seconds_to_nanoseconds(204);
        let target_level: f32 = 50.0;

        let result = rl.apply_rate_limit(target_level, false, t3);

        assert_eq!(result, 49.0, "Level should advance by -2.0% in 16 seconds.");
        assert_eq!(
            rl.rl_ssoc_last_update,
            seconds_to_nanoseconds(204),
            "Last update time should be 172."
        );
        assert_eq!(rl.rl_current_level, 49.0);
    }

    #[fuchsia::test]
    fn test_rate_limiter_called_by_polisher() {
        let mut polisher = Polisher::new();

        let initial_level = 51.5;
        let mut incoming_info = new_info(initial_level, fpower::ChargeStatus::Charging);
        incoming_info.timestamp = Some(0);

        let info = polisher.polish_info(incoming_info);
        let initial_scaled_level = InitialScaler::scale_level(initial_level);
        assert_matches!(info.level_percent, Some(p) if (p - initial_scaled_level).abs() < f32::EPSILON);

        let t0_s = 30;
        let t0: TimeStampNs = seconds_to_nanoseconds(t0_s);
        let new_level: f32 = 60.0;
        let mut incoming_info = new_info(new_level, fpower::ChargeStatus::Charging);
        incoming_info.timestamp = Some(t0);

        let info = polisher.polish_info(incoming_info);
        let expected_level2 = initial_scaled_level
            + t0_s as f32 * RateLimiter::RL_MAX_DELTA_SOC / RateLimiter::RL_MAX_TIME_S;
        assert_matches!(info.level_percent, Some(level) if expected_level2 == level);
    }

    #[fuchsia::test]
    fn test_polish_info_for_full_cycle() {
        let mut polisher = Polisher::new();
        // Establish that we are charging and unmodified.
        let info = polisher.polish_info(new_info(95.0, fpower::ChargeStatus::Charging));
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Unmodified);
        let expected_level = InitialScaler::scale_level(95.0);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);

        // Reaching 100%, then unplug at 96%, and expect 100% spoofed.
        let _ = polisher.polish_info(new_info(100.0, fpower::ChargeStatus::Full));
        let info = polisher.polish_info(new_info(96.0, fpower::ChargeStatus::Discharging));
        let expected_level = InitialScaler::scale_level(100.0);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Spoofing);

        // Drop to 90%
        let info = polisher.polish_info(new_info(90.0, fpower::ChargeStatus::Discharging));
        let expected_level = InitialScaler::scale_level(90.0);
        let expected_level = CurveMapper::splice_for_level(
            expected_level,
            CurvePoint { real: CurveMapper::LEVEL_TRUE, ui: CurveMapper::LEVEL_TRUE },
            CurvePoint { real: CurveMapper::LEVEL_SPOOF, ui: CurveMapper::LEVEL_FULL },
        );
        info!("expected leve = {:?}", expected_level);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);
        assert_matches!(
            polisher.curve_mapper.curve_state,
            CurveState::Splicing(cp) => {
                // Logic you want to run ONLY if it matches Splicing
                assert_eq!(cp.ui, CurveMapper::LEVEL_FULL);
                assert_eq!(cp.real, CurveMapper::LEVEL_SPOOF);
            }
        );

        // Back to Unmodified
        let info = polisher.polish_info(new_info(14.0, fpower::ChargeStatus::Discharging));
        let expected_level = InitialScaler::scale_level(14.0);
        assert_matches!(info.level_percent, Some(p) if (p - expected_level).abs() < f32::EPSILON);
        assert_matches!(polisher.curve_mapper.curve_state, CurveState::Unmodified);
    }

    #[fuchsia::test]
    fn test_polish_info() {
        let mut polisher = Polisher::new();
        info!(" original mid point: {:?}", polisher.curve_mapper.curve_state);

        // Test when level_percent = shutdown offset
        let mut info = fpower::BatteryInfo {
            level_percent: Some(InitialScaler::SHUTDOWN_OFFSET),
            charge_status: Some(fpower::ChargeStatus::Discharging),
            ..Default::default()
        };
        info = polisher.polish_info(info);
        assert_eq!(info.level_percent, Some(0.0));
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Critical));

        // Test a dead battery
        info.level_percent = Some(0.0);
        info.charge_status = Some(fpower::ChargeStatus::Charging);
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Low));

        // Test a battery that is charging
        info.level_percent = Some(10.0);
        info.charge_status = Some(fpower::ChargeStatus::Charging);
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Low));

        info.level_percent = Some(49.0);
        info.charge_status = Some(fpower::ChargeStatus::Charging);
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Warning));

        info.level_percent = Some(49.0);
        info.charge_status = Some(fpower::ChargeStatus::Discharging);
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Warning));

        info.level_percent = Some(83.0);
        info.charge_status = Some(fpower::ChargeStatus::Charging);
        info.average_charging_current_ua =
            Some(ChargeTimeEstimator::get_reference_current_ua(83.0, None));
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Ok));
        // Expected nanoseconds (1432 seconds * 1,000,000,000 nanos/sec)
        const EXPECTED_NANOS: i64 = 1_340_000_000_000;
        assert_eq!(info.time_remaining, Some(fpower::TimeRemaining::FullCharge(EXPECTED_NANOS)));

        // Test when level_percent = 100%
        info.level_percent = Some(100.0);
        info = polisher.polish_info(info);
        assert_eq!(info.level_percent, Some(100.0));
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Ok));
        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(0)),
            "When level is None, time_remaining must be set to Indeterminate(0)."
        );
    }
}
