// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use argh::FromArgs;

#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "list")]
/// List input devices.
pub struct ListArgs {}

pub async fn run(_args: ListArgs) -> Result<()> {
    println!("list subcommand called");
    Ok(())
}
