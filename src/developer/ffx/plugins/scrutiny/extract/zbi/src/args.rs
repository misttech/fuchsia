// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;
use std::path::PathBuf;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "zbi",
    description = "Extracts the Zircon Boot Image",
    example = "To extract a Zircon Boot Image:

        $ffx scrutiny extract zbi foo.zbi /tmp/foo",
    note = "Extracts a ZBI to a specific directory."
)]
pub struct ScrutinyZbiCommand {
    #[argh(positional)]
    pub input: PathBuf,
    #[argh(positional)]
    pub output: PathBuf,
}
