// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "discover",
    description = "Discovers targets",
    note = "Discovers targets, storing them in the discovery cache. By default, runs as a background process."
)]
pub struct DiscoverCommand {
    #[argh(switch, short = 'f', description = "run in the foreground")]
    pub foreground: bool,

    #[argh(switch, short = 'q', description = "do not write to stdout")]
    pub quiet: bool,

    #[argh(
        option,
        short = 't',
        description = "time in seconds between updates (default: 60). 0 to run once (only valid in foreground)"
    )]
    pub time: Option<u64>,

    #[argh(
        switch,
        description = "stop the discover process (whether in foreground or background)"
    )]
    pub stop: bool,
}
