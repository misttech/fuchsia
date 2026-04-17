// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::prelude_internal::*;

/// This structure represents a Thread network info in the history tracker report.
///
/// Functional equivalent of [`otsys::otHistoryTrackerNetworkInfo`](crate::otsys::otHistoryTrackerNetworkInfo).
#[derive(Debug, Default, Clone)]
#[repr(transparent)]
pub struct HistoryTrackerNetworkInfo(pub otHistoryTrackerNetworkInfo);

impl_ot_castable!(HistoryTrackerNetworkInfo, otHistoryTrackerNetworkInfo);

impl HistoryTrackerNetworkInfo {
    /// Returns the device Role.
    pub fn role(&self) -> DeviceRole {
        DeviceRole::from(self.0.mRole)
    }

    /// Returns the device's MLE link mode.
    pub fn mode(&self) -> LinkModeConfig {
        LinkModeConfig::from(self.0.mMode)
    }

    /// Returns the device's RLOC16.
    pub fn rloc16(&self) -> u16 {
        self.0.mRloc16
    }

    /// Returns the Thread network partition ID.
    pub fn partition_id(&self) -> u32 {
        self.0.mPartitionId
    }
}
