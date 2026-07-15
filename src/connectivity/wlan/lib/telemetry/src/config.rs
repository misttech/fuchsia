// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub enable_connect_disconnect: bool,
    pub enable_iface_logger: bool,
    pub enable_power_logger: bool,
    pub enable_recovery_logger: bool,
    pub enable_scan_logger: bool,
    pub enable_pno_scan_logger: bool,
    pub enable_sme_timeout_logger: bool,
    pub enable_toggle_logger: bool,
    pub enable_tx_power_scenario_logger: bool,
    pub enable_client_iface_counters_logger: bool,
}

impl TelemetryConfig {
    pub fn all() -> Self {
        Self {
            enable_connect_disconnect: true,
            enable_iface_logger: true,
            enable_power_logger: true,
            enable_recovery_logger: true,
            enable_scan_logger: true,
            enable_pno_scan_logger: true,
            enable_sme_timeout_logger: true,
            enable_toggle_logger: true,
            enable_tx_power_scenario_logger: true,
            enable_client_iface_counters_logger: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CobaltAllowlist {
    All,
    Only(HashSet<u32>),
}

impl CobaltAllowlist {
    pub fn contains(&self, metric_id: u32) -> bool {
        match self {
            CobaltAllowlist::All => true,
            CobaltAllowlist::Only(set) => set.contains(&metric_id),
        }
    }
}
