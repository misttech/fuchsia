// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fpower;

const SHUTDOWN_OFFSET: f32 = 3.0;

// Used to determine the level_status, using polished level after scale_level
const THRESHOLD_LEVEL_OK: f32 = 80.0;
const THRESHOLD_LEVEL_WARNING: f32 = 30.0;
const THRESHOLD_LEVEL_LOW: f32 = 0.0;

fn scale_level(level: f32, shutdown_offset: f32) -> f32 {
    if level >= 100.0 {
        return 100.0;
    } else if level >= shutdown_offset {
        return (level - shutdown_offset) * 100.0 / (100.0 - shutdown_offset);
    } else {
        return 0.0;
    }
}

fn determine_level_status(
    level: f32,
    charger_status: Option<fpower::ChargeStatus>,
) -> fpower::LevelStatus {
    if level > THRESHOLD_LEVEL_OK {
        return fpower::LevelStatus::Ok;
    } else if level > THRESHOLD_LEVEL_WARNING {
        return fpower::LevelStatus::Warning;
    } else if level > THRESHOLD_LEVEL_LOW {
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

pub(crate) struct Polisher {
    shutdown_offset: f32,
}

impl Polisher {
    pub fn new() -> Polisher {
        Polisher { shutdown_offset: SHUTDOWN_OFFSET }
    }

    fn scale_battery_info_for_shutdown(&self, info: &mut fpower::BatteryInfo) {
        if let Some(level) = info.level_percent {
            info.level_percent = Some(scale_level(level as f32, self.shutdown_offset));
        }
    }

    fn set_level_status(&self, info: &mut fpower::BatteryInfo) {
        if let Some(level) = info.level_percent {
            info.level_status = Some(determine_level_status(level as f32, info.charge_status));
        }
    }

    pub fn polish_info(&self, info: fpower::BatteryInfo) -> fpower::BatteryInfo {
        let mut info = info;
        self.scale_battery_info_for_shutdown(&mut info);
        self.set_level_status(&mut info);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_level() {
        assert_eq!(scale_level(SHUTDOWN_OFFSET, SHUTDOWN_OFFSET), 0.0);
        assert_eq!(scale_level(100.0, SHUTDOWN_OFFSET), 100.0);
    }

    #[test]
    fn test_determine_level_status() {
        assert_eq!(
            determine_level_status(0.0, Some(fpower::ChargeStatus::NotCharging)),
            fpower::LevelStatus::Critical
        );
        assert_eq!(
            determine_level_status(0.0, Some(fpower::ChargeStatus::Discharging)),
            fpower::LevelStatus::Critical
        );
        assert_eq!(
            determine_level_status(0.0, Some(fpower::ChargeStatus::Unknown)),
            fpower::LevelStatus::Unknown
        );
        assert_eq!(determine_level_status(0.0, None), fpower::LevelStatus::Unknown);
        assert_eq!(determine_level_status(THRESHOLD_LEVEL_OK + 1.0, None), fpower::LevelStatus::Ok);
        assert_eq!(
            determine_level_status(THRESHOLD_LEVEL_WARNING + 1.0, None),
            fpower::LevelStatus::Warning
        );
        assert_eq!(
            determine_level_status(THRESHOLD_LEVEL_LOW + 1.0, None),
            fpower::LevelStatus::Low
        );
        assert_eq!(
            determine_level_status(100.0, Some(fpower::ChargeStatus::Charging)),
            fpower::LevelStatus::Ok
        );
    }

    #[test]
    fn test_scale_battery_info_for_shutdown() {
        let polisher = Polisher::new();

        // Test when level_percent = shutdown offset
        let mut info = fpower::BatteryInfo {
            level_percent: Some(polisher.shutdown_offset),
            ..Default::default()
        };
        polisher.scale_battery_info_for_shutdown(&mut info);
        assert_eq!(info.level_percent, Some(0.0));

        // Test when level_percent = 100%
        info.level_percent = Some(100.0);
        polisher.scale_battery_info_for_shutdown(&mut info);
        assert_eq!(info.level_percent, Some(100.0));
    }

    #[test]
    fn test_polish_info() {
        let polisher = Polisher::new();

        // Test when level_percent = shutdown offset
        let mut info = fpower::BatteryInfo {
            level_percent: Some(polisher.shutdown_offset),
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
