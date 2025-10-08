// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Utilities to make interacting with `fuchsia.net.policy.properties` more ergonomic.

#![deny(unused)]
#![deny(missing_docs)]

use fidl_fuchsia_net_policy_properties as fnp_properties;
use zx::HandleBased as _;

/// Extensions for [`fnp_properties::NetworkToken`].
pub trait NetworkTokenExt: Sized {
    /// Attempt to make a copy of the current [`fnp_properties::NetworkToken`],
    /// to allow it to be passed to a
    /// `fuchsia.net.policy.properties/Networks.WatchProperties` call.
    fn duplicate(&self) -> Result<Self, zx::Status>;
}

impl NetworkTokenExt for fnp_properties::NetworkToken {
    fn duplicate(&self) -> Result<fnp_properties::NetworkToken, zx::Status> {
        Ok(fnp_properties::NetworkToken {
            value: Some(
                self.value
                    .as_ref()
                    .ok_or(zx::Status::NOT_FOUND)?
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            ),
            ..Default::default()
        })
    }
}
