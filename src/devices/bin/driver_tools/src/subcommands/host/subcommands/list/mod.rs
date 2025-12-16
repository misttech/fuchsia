// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::Result;
use args::ListCommand;
use fidl_fuchsia_driver_development as fdd;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

pub async fn list(
    _cmd: ListCommand,
    w: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    let device_info = fuchsia_driver_dev::get_device_info(
        &driver_development_proxy,
        &[],
        /* exact_match= */ false,
    )
    .await?;

    let mut driver_hosts = BTreeMap::new();

    for device in device_info {
        if let Some(koid) = device.driver_host_koid {
            if let Some(url) = device.bound_driver_url {
                driver_hosts.entry(koid).or_insert(BTreeSet::new()).insert(url);
            }
        }
    }

    for (koid, drivers) in driver_hosts {
        if termion::is_tty(&std::io::stdout()) {
            writeln!(w, "Driver Host: {}", koid)?;
            for driver in drivers {
                writeln!(w, "{:>4}{}", "", driver)?;
            }
            writeln!(w, "")?;
        } else {
            for driver in drivers {
                writeln!(w, "Driver Host: {:<6}{}", koid, driver)?;
            }
        }
    }
    Ok(())
}
