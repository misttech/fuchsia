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

    let driver_host_info =
        fuchsia_driver_dev::get_driver_host_info(&driver_development_proxy).await?;

    let mut driver_host_drivers = BTreeMap::new();

    for device in device_info {
        if let Some(koid) = device.driver_host_koid
            && let Some(url) = device.bound_driver_url
        {
            driver_host_drivers.entry(koid).or_insert(BTreeSet::new()).insert(url);
        }
    }

    let mut driver_hosts_names = BTreeMap::new();

    for host in driver_host_info {
        if let Some(koid) = host.process_koid
            && let Some(name) = host.name
            && !name.is_empty()
        {
            driver_hosts_names.insert(koid, name);
        }
    }

    for (koid, drivers) in driver_host_drivers {
        if termion::is_tty(&std::io::stdout()) {
            writeln!(w, "Driver Host: {}", koid)?;
            if let Some(name) = driver_hosts_names.get(&koid) {
                writeln!(w, "Name: {}", name)?;
            }
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
