// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::{Context, Result};
use args::WriteCommand;
use std::io::Write;
use {fidl_fuchsia_hardware_i2c as fi2c, fidl_fuchsia_io as fio, zx_status as zx};

pub async fn write(
    cmd: &WriteCommand,
    writer: &mut dyn Write,
    dev: &fio::DirectoryProxy,
) -> Result<()> {
    let device = super::connect_to_i2c_device(&cmd.device_path, dev)
        .context("Failed to connect to I2C device")?;
    let transactions = &[fi2c::Transaction {
        data_transfer: Some(fi2c::DataTransfer::WriteData(cmd.data.clone())),
        ..Default::default()
    }];
    device
        .transfer(transactions)
        .await
        .context("Failed to send request to transfer write transaction to I2C device")?
        .map_err(|status| zx::Status::from_raw(status))
        .context("Failed to transfer write transaction to I2C device")?;
    write!(writer, "Write:")?;
    for byte in cmd.data.iter() {
        write!(writer, " {:#04x}", byte)?;
    }
    writeln!(writer, "")?;
    Ok(())
}
