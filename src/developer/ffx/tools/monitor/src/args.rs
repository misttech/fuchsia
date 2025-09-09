// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "monitor",
    description = "Start a local server to monitor status.",
    example = "ffx monitor start --port 8080"
)]
pub struct MonitorCommand {
    #[argh(subcommand)]
    pub subcommand: SubCommand,
}

#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
pub enum SubCommand {
    Start(StartCommand),
    Stop(StopCommand),
    Status(StatusCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "start", description = "Start the monitor server")]
pub struct StartCommand {}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "stop", description = "Stop the monitor server")]
pub struct StopCommand {}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "status", description = "Check the statuses")]
pub struct StatusCommand {}
