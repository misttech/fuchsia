// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fidl_fuchsia_power_system as fsystem;
use log::warn;

pub struct WakeLease {
    _token: fsystem::LeaseToken,
}

impl WakeLease {
    /// Constructs a WakeLease from an underlying token.
    /// This is intended strictly for use in test environments.
    #[doc(hidden)]
    pub fn from_token_for_test(token: fsystem::LeaseToken) -> Self {
        Self { _token: token }
    }
}

#[async_trait]
pub trait PowerManager: Send + Sync {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease>;
}

pub struct DevicePowerManager {
    activity_governor: Option<fsystem::ActivityGovernorProxy>,
}

impl DevicePowerManager {
    pub fn new(activity_governor: Option<fsystem::ActivityGovernorProxy>) -> Self {
        Self { activity_governor }
    }
}

#[async_trait]
impl PowerManager for DevicePowerManager {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease> {
        let activity_governor = self.activity_governor.as_ref()?;
        match activity_governor.acquire_wake_lease(name).await {
            Ok(Ok(token)) => Some(WakeLease { _token: token }),
            Ok(Err(e)) => {
                warn!("Failed to acquire wake lease {}: {:?}", name, e);
                None
            }
            Err(e) => {
                warn!("FIDL error when acquiring wake lease {}: {:?}", name, e);
                None
            }
        }
    }
}
