// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::Result;
use args::{PidOrName, ShowCommand};
use fidl_fuchsia_driver_development as fdd;
use std::collections::BTreeSet;
use std::io::Write;

pub async fn show(
    cmd: ShowCommand,
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

    let mut drivers = BTreeSet::new();
    let mut devices = Vec::new();

    let Some(driver_host) = driver_host_info.iter().find(|info| match &cmd.pid_or_name {
        PidOrName::Pid(pid) => info.process_koid.as_ref().unwrap() == pid,
        PidOrName::Name(name) => info.name.as_ref().unwrap() == name,
    }) else {
        anyhow::bail!("driver host not found");
    };

    for device in device_info {
        if let Some(koid) = device.driver_host_koid
            && koid == driver_host.process_koid.unwrap()
        {
            if let Some(url) = device.bound_driver_url {
                drivers.insert(url);
            }
            if let Some(moniker) = device.moniker {
                devices.push(moniker);
            }
        }
    }

    // TODO: Handle cmd.runtime
    // TODO: Handle cmd.stack_trace

    if let Some(name) = &driver_host.name
        && !name.is_empty()
    {
        writeln!(w, "Name: {name}")?;
    }
    if let Some(koid) = &driver_host.process_koid {
        writeln!(w, "PID:  {koid}")?;
        writeln!(w, "")?;
    }

    writeln!(w, "Drivers:")?;
    for driver in drivers {
        writeln!(w, "{:>4}{}", "", driver)?;
    }
    writeln!(w, "")?;
    writeln!(w, "Devices:")?;
    for device in devices {
        writeln!(w, "{:>4}{}", "", device)?;
    }
    Ok(())
}
