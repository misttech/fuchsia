// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl_fuchsia_power_topology_test as fpt;

pub trait Connector {
    fn get_system_activity_control(
        &self,
    ) -> impl std::future::Future<Output = Result<fpt::SystemActivityControlProxy>>;
}
