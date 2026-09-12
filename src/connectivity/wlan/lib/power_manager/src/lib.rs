// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use async_trait::async_trait;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_system as fsystem;
use log::warn;

pub const POWER_LEVEL_OFF: u8 = 0;
pub const POWER_LEVEL_SUSPEND: u8 = 1;
pub const POWER_LEVEL_ACTIVE: u8 = 2;

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
    async fn power_element_lease(
        &self,
        lease_name: &str,
        dependency_token: fbroker::DependencyToken,
        dependency_level: u8,
    ) -> Result<fbroker::LeaseToken, Error>;
    async fn register_suspend_blocker(
        &self,
        suspend_blocker: fidl::endpoints::ClientEnd<fsystem::SuspendBlockerMarker>,
        name: &str,
    ) -> Result<WakeLease, Error>;
}

pub struct DevicePowerManager {
    proxies: Option<(fsystem::ActivityGovernorProxy, fbroker::TopologyProxy)>,
}

impl DevicePowerManager {
    pub fn new(proxies: Option<(fsystem::ActivityGovernorProxy, fbroker::TopologyProxy)>) -> Self {
        Self { proxies }
    }
}

#[async_trait]
impl PowerManager for DevicePowerManager {
    async fn take_wake_lease(&self, name: &str) -> Option<WakeLease> {
        let (activity_governor, _) = self.proxies.as_ref()?;
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

    async fn power_element_lease(
        &self,
        lease_name: &str,
        dependency_token: fbroker::DependencyToken,
        dependency_level: u8,
    ) -> Result<fbroker::LeaseToken, Error> {
        let (_, power_broker) =
            self.proxies.as_ref().ok_or_else(|| anyhow::anyhow!("Power broker not available"))?;

        let dep = fbroker::LeaseDependency {
            requires_token: Some(dependency_token),
            requires_level: Some(dependency_level),
            ..Default::default()
        };
        let (lease_token_local, lease_token_server) = zx::EventPair::create();
        let schema = fbroker::LeaseSchema {
            lease_token: Some(lease_token_server),
            lease_name: Some(lease_name.to_string()),
            dependencies: Some(vec![dep]),
            ..Default::default()
        };
        power_broker
            .lease(schema)
            .await?
            .map_err(|e| anyhow::anyhow!("Power broker returned error: {:?}", e))?;
        Ok(lease_token_local)
    }

    async fn register_suspend_blocker(
        &self,
        suspend_blocker: fidl::endpoints::ClientEnd<fsystem::SuspendBlockerMarker>,
        name: &str,
    ) -> Result<WakeLease, Error> {
        let (activity_governor, _) = self
            .proxies
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Activity governor not available"))?;
        let payload = fsystem::ActivityGovernorRegisterSuspendBlockerRequest {
            suspend_blocker: Some(suspend_blocker),
            name: Some(name.to_string()),
            ..Default::default()
        };
        match activity_governor.register_suspend_blocker(payload).await {
            Ok(Ok(token)) => Ok(WakeLease { _token: token }),
            Ok(Err(e)) => Err(anyhow::anyhow!("RegisterSuspendBlocker error: {:?}", e)),
            Err(e) => Err(anyhow::anyhow!("FIDL error: {:?}", e)),
        }
    }
}
