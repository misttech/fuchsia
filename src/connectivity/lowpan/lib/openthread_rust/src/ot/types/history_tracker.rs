// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::prelude_internal::*;

use num_derive::FromPrimitive;

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

/// Defines the events in a neighbor info (i.e. whether neighbor is added, removed, or changed).
///
/// Functional equivalent of [`otsys::otHistoryTrackerNeighborEvent`](crate::otsys::otHistoryTrackerNeighborEvent).
#[derive(Debug, Copy, Clone, Eq, Ord, PartialOrd, PartialEq, FromPrimitive)]
#[allow(missing_docs)]
pub enum HistoryTrackerNeighborEvent {
    /// Functional equivalent of [`otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_ADDED`](crate::otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_ADDED).
    Added = OT_HISTORY_TRACKER_NEIGHBOR_EVENT_ADDED as isize,

    /// Functional equivalent of [`otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_REMOVED`](crate::otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_REMOVED).
    Removed = OT_HISTORY_TRACKER_NEIGHBOR_EVENT_REMOVED as isize,

    /// Functional equivalent of [`otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_CHANGED`](crate::otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_CHANGED).
    Changed = OT_HISTORY_TRACKER_NEIGHBOR_EVENT_CHANGED as isize,

    /// Functional equivalent of [`otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_RESTORING`](crate::otsys::OT_HISTORY_TRACKER_NEIGHBOR_EVENT_RESTORING).
    Restoring = OT_HISTORY_TRACKER_NEIGHBOR_EVENT_RESTORING as isize,
}

impl From<otHistoryTrackerNeighborEvent> for HistoryTrackerNeighborEvent {
    fn from(x: otHistoryTrackerNeighborEvent) -> Self {
        use num::FromPrimitive;
        Self::from_u32(x)
            .unwrap_or_else(|| panic!("Unknown otHistoryTrackerNeighborEvent value: {x}"))
    }
}

/// This structure represents a Thread neighbor info in the history tracker report.
///
/// Functional equivalent of [`otsys::otHistoryTrackerNeighborInfo`](crate::otsys::otHistoryTrackerNeighborInfo).
#[derive(Debug, Default, Clone)]
#[repr(transparent)]
pub struct HistoryTrackerNeighborInfo(pub otHistoryTrackerNeighborInfo);

impl_ot_castable!(HistoryTrackerNeighborInfo, otHistoryTrackerNeighborInfo);

impl HistoryTrackerNeighborInfo {
    /// Returns Neighbor's Extended Address.
    pub fn ext_address(&self) -> ExtAddress {
        ExtAddress(self.0.mExtAddress)
    }

    /// Returns the Neighbor's RLOC16.
    pub fn rloc16(&self) -> u16 {
        self.0.mRloc16
    }

    /// Returns the Average RSSI of rx frames from neighbor at the time of recording entry.
    pub fn avg_rssi(&self) -> i8 {
        self.0.mAverageRssi
    }

    /// Returns the neighbor event (`OT_HISTORY_TRACKER_NEIGHBOR_EVENT_*` enumeration).
    pub fn event(&self) -> HistoryTrackerNeighborEvent {
        HistoryTrackerNeighborEvent::from(self.0.mEvent() as u32)
    }

    /// Returns true if the neighbor has its receiver on when not transmitting.
    pub fn rx_on_while_idle(&self) -> bool {
        self.0.mRxOnWhenIdle()
    }

    /// Returns true if the neighbor is a Full Thread Device.
    pub fn full_thread_device(&self) -> bool {
        self.0.mFullThreadDevice()
    }

    /// Returns true if the neighbor requires the full Network Data.
    pub fn full_network_data(&self) -> bool {
        self.0.mFullNetworkData()
    }

    /// Returns true if the neighbor is a child, otherwise, is a router.
    pub fn is_child(&self) -> bool {
        self.0.mIsChild()
    }
}
