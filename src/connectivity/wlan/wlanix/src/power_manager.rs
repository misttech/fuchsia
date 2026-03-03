// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fidl_fuchsia_power_system as fsystem;
use log::warn;

pub(crate) struct WakeLease {
    pub(crate) _token: fsystem::LeaseToken,
}

#[async_trait]
pub(crate) trait PowerManager: Send + Sync {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease>;
}

pub(crate) struct DevicePowerManager {
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

#[cfg(test)]
use {fuchsia_sync::Mutex, std::sync::Arc};

#[cfg(test)]
pub(crate) struct TestPowerManager {
    pub calls: Arc<Mutex<Vec<String>>>,
    pub active_leases: Arc<Mutex<Vec<(String, fsystem::LeaseToken)>>>,
}

#[cfg(test)]
impl TestPowerManager {
    pub fn new() -> Self {
        Self { calls: Arc::new(Mutex::new(vec![])), active_leases: Arc::new(Mutex::new(vec![])) }
    }

    pub fn is_lease_dropped(&self, name: &str) -> bool {
        let leases = self.active_leases.lock();
        if let Some((_, token)) = leases.iter().rev().find(|(n, _)| n == name) {
            // Check if PEER_CLOSED is already signaled
            #[allow(clippy::match_like_matches_macro)]
            match token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
            {
                zx::WaitResult::TimedOut(_) => false,
                _ => true,
            }
        } else {
            true // If it was never created, it can be considered dropped
        }
    }
}

#[cfg(test)]
#[async_trait]
impl PowerManager for TestPowerManager {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease> {
        self.calls.lock().push(name.to_string());
        let (local_token, token) = fsystem::LeaseToken::create();
        self.active_leases.lock().push((name.to_string(), local_token));
        Some(WakeLease { _token: token })
    }
}
