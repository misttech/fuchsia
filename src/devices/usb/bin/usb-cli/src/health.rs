// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_usb_policy as fpolicy;
use fidl_fuchsia_usb_policy as usb_policy;
use std::fmt;

struct DisplayableDeviceState(fpolicy::DeviceState);

impl fmt::Display for DisplayableDeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            fpolicy::DeviceState::NotAttached => write!(f, "Not Attached"),
            fpolicy::DeviceState::Attached => write!(f, "Attached"),
            fpolicy::DeviceState::Powered => write!(f, "Powered"),
            fpolicy::DeviceState::Default => write!(f, "Default"),
            fpolicy::DeviceState::Address => write!(f, "Address"),
            fpolicy::DeviceState::Configured => write!(f, "Configured"),
            fpolicy::DeviceState::Suspended => write!(f, "Suspended"),
            _ => write!(f, "Unknown"),
        }
    }
}

pub fn format_report(report: &usb_policy::HealthReport) -> String {
    let mut out = String::from("USB Health Report\n");
    if let Some(state) = report.state {
        out.push_str(&format!("  Controller: {}\n", DisplayableDeviceState(state)));
    } else {
        out.push_str("  Controller: Unknown\n");
    }
    if let Some(address) = report.address {
        out.push_str(&format!("  Address: {}", address));
    } else {
        out.push_str("  Address: Unknown");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_report_known() {
        let report = usb_policy::HealthReport {
            state: Some(fpolicy::DeviceState::Configured),
            address: Some(42),
            ..Default::default()
        };
        assert_eq!(
            format_report(&report),
            "USB Health Report\n  Controller: Configured\n  Address: 42"
        );
    }

    #[test]
    fn test_format_report_unknown() {
        let report = usb_policy::HealthReport { state: None, address: None, ..Default::default() };
        assert_eq!(
            format_report(&report),
            "USB Health Report\n  Controller: Unknown\n  Address: Unknown"
        );
    }

    #[rustfmt::skip]
    #[test]
    fn test_displayable_device_state() {
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::NotAttached)), "Not Attached");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Attached)), "Attached");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Powered)), "Powered");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Default)), "Default");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Address)), "Address");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Configured)), "Configured");
        assert_eq!(format!("{}", DisplayableDeviceState(fpolicy::DeviceState::Suspended)), "Suspended");
    }
}
