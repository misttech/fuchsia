// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fpower;

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

pub(crate) struct Polisher {}

impl Polisher {
    pub fn new() -> Polisher {
        Polisher {}
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

    pub fn polish_info(&self, info: fpower::BatteryInfo) -> fpower::BatteryInfo {
        let mut info = info;
        self.scale_battery_level(&mut info);
        self.set_level_status(&mut info);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[fuchsia::test]
    fn test_polish_info() {
        let polisher = Polisher::new();

        // Test when level_percent = shutdown offset
        let mut info = fpower::BatteryInfo {
            level_percent: Some(InitialScaler::SHUTDOWN_OFFSET),
            charge_status: Some(fpower::ChargeStatus::Discharging),
            ..Default::default()
        };
        info = polisher.polish_info(info);
        assert_eq!(info.level_percent, Some(0.0));
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Critical));

        info.level_percent = Some(83.0);
        info.charge_status = Some(fpower::ChargeStatus::Charging);
        info = polisher.polish_info(info);
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Ok));

        // Test when level_percent = 100%
        info.level_percent = Some(100.0);
        info = polisher.polish_info(info);
        assert_eq!(info.level_percent, Some(100.0));
        assert_eq!(info.level_status, Some(fpower::LevelStatus::Ok));
    }
}
