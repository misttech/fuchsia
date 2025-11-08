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
///                                  |=====> Time to Full
///                                  |
///                                  V
///                               Filters
///                                  |
///                                  V
///                        Spoofing and Splicing
///                                  |
///                                  V
///                           rate limiting
///                                  |
///                                  V
///                  Reported to upper level, displayed

struct InitialScaler;

impl InitialScaler {
    // Scale the initial battery level from 3-100% to 0-100%
    const SHUTDOWN_OFFSET: f32 = 3.0;
    fn scale_level(level: f32) -> f32 {
        if level >= 100.0 {
            return 100.0;
        } else if level >= Self::SHUTDOWN_OFFSET {
            return (level - Self::SHUTDOWN_OFFSET) * 100.0 / (100.0 - Self::SHUTDOWN_OFFSET);
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
    // (duration, threshold) stores number of seconds to gain 1% charge, at level <= corresponding
    // threshold. For 0-78%, the duration = 32 seconds. For 79-86, it's 56 seconds.
    // TODO(https://fxbug.dev/442619993): Read this table from a device tree or a configuration.
    const PERCENT_CHARGE_DURATION: [(i32, u32); 4] = [(32, 78), (56, 86), (84, 96), (92, 100)];

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

    fn time_to_full(&self, current_level: f32) -> zx::BootDuration {
        let total_time_s: i32 =
            (current_level.ceil() as u32..=100).map(|level| self.get_level_duration(level)).sum();

        // Convert to nanoseconds
        zx::Duration::from_seconds(total_time_s as i64)
    }

    // Predict the time in seconds needed to charge to next level (1% above)
    fn get_level_duration(&self, level: u32) -> i32 {
        let level = level as usize;
        if level >= LOOKUP_TABLE_SIZE {
            return 0;
        }
        self.baseline_duration_lookup[level]
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
                } else if charge_status == Some(fpower::ChargeStatus::Full) {
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
                } else if info.charge_status == Some(fpower::ChargeStatus::Discharging) {
                    Self::splice_for_discharging(level, mid_point)
                } else {
                    warn!("Something is wrong, we are not charging, neither are we discharging");
                    level
                }
            }
            _ => level,
        };

        self.prev_ui_level = level;
        self.prev_charge_status = info.charge_status;

        info.level_percent = Some(new_level);
    }
}

pub(crate) struct Polisher {
    curve_mapper: CurveMapper,
    last_level: Option<f32>,
    estimator: ChargeTimeEstimator,
}

impl Polisher {
    pub fn new() -> Polisher {
        Polisher {
            curve_mapper: CurveMapper::new(),
            last_level: None,
            estimator: ChargeTimeEstimator::new(),
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
            info!("level none");
            info.time_remaining = Some(fpower::TimeRemaining::Indeterminate(0));
            return;
        };
        if info.charge_status != Some(fpower::ChargeStatus::Charging) {
            info!("charge status not charging");
            info.time_remaining = Some(fpower::TimeRemaining::Indeterminate(0));
            return;
        }

        let time_to_full_estimate = self.estimator.time_to_full(level).into_nanos();
        info.time_remaining = Some(fpower::TimeRemaining::FullCharge(time_to_full_estimate));
    }

    pub fn polish_info(&mut self, info: fpower::BatteryInfo) -> fpower::BatteryInfo {
        let original_level = info.level_percent;
        let mut info = info;
        self.scale_battery_level(&mut info);
        self.set_level_status(&mut info);
        let scaled_level = info.level_percent;
        self.calculate_time_to_full(&mut info);
        self.process_curve_state(&mut info);
        if self.last_level != original_level {
            info!(
                "Levels - original: {:?}, scaled: {:?}, post curve mapping: {:?}",
                original_level, scaled_level, info.level_percent
            );
            self.last_level = original_level;
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[fuchsia::test]
    fn test_scale_level() {
        assert_eq!(InitialScaler::scale_level(InitialScaler::SHUTDOWN_OFFSET), 0.0);
        assert_eq!(InitialScaler::scale_level(51.5), 50.0);
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
        assert_eq!(estimator.time_to_full(100.0).into_seconds(), 0);

        // --- CASE 2: Near Full (99.0) ---
        // Sums: 99, 100 (2 levels) -> Call(99)=92s, Call(100)=0s. Total: 92s.
        let expected_99 = 92;
        assert_eq!(estimator.time_to_full(99.0).into_seconds(), expected_99);

        // --- CASE 3: Level 91.0 (Starts sum at 91) ---
        // Levels 91-96 (6 * 84s) + 97-99 (3 * 92s) + 100 (0s) = 504 + 276 = 780 seconds
        let expected_91 = 780;
        assert_eq!(estimator.time_to_full(91.0).into_seconds(), expected_91);

        // --- CASE 4: Level 85.0 (Starts sum at 85)
        // Levels 85-86 (2 * 56s) + Levels 87-100 (10*84 + 3*92 + 0)
        // Sums: (2 * 56) + 840 + 276 = 112 + 1116 = 1228 seconds
        let expected_85 = 1228;
        assert_eq!(
            estimator.time_to_full(85.0).into_seconds(),
            expected_85,
            "At 85%, time remaining should be 1228 seconds."
        );

        // --- CASE 5: Level 50.0 (Starts sum at 50)
        // Levels 50-78 (29 * 32s) + Levels 79-100 (448 + 840 + 276)
        // Sums: (29 * 32) + 1564 = 928 + 1564 = 2492 seconds
        let expected_50 = 2492;
        assert_eq!(
            estimator.time_to_full(50.0).into_seconds(),
            expected_50,
            "At 50%, time remaining should be 2492 seconds."
        );

        // --- CASE 6: Level 0.0% ---
        // Total seconds: (4184 - 92) = 4092 seconds.
        let expected_0 = 4092;
        assert_eq!(
            estimator.time_to_full(0.0).into_seconds(),
            expected_0,
            "At 0%, time remaining should be 4092 seconds."
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

        // Test 50%
        let expected_50 = 2492 * NANOS_PER_SEC;
        info = new_info(50.0, fpower::ChargeStatus::Charging);
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(expected_50)),
            "At 100%, time remaining should be 0 seconds."
        );

        // Test 100%
        info = new_info(100.0, fpower::ChargeStatus::Charging);
        polisher.calculate_time_to_full(&mut info);
        assert_eq!(
            info.time_remaining,
            Some(fpower::TimeRemaining::FullCharge(0)),
            "At 100%, time remaining should be 0 seconds."
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
