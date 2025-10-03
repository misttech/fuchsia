// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use arg_parsing::parse_duration;
use argh::{ArgsInfo, FromArgs};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "application-activity",
    description = "Takes and drops leases on activity governor elements.",
    example = "\
To change application_activity power level to 1:

    $ power system-activity application-activity start

    To change application_activity power level to 0:

    $ power system-activity application-activity stop",
    note = "\
If the system-activity-governor-controller component is not available, then this command will not
work properly."
)]

pub struct ApplicationActivityCommand {
    #[argh(subcommand)]
    pub subcommand: ApplicationActivitySubcommand,
}

#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
pub enum ApplicationActivitySubcommand {
    Start(StartCommand),
    Stop(StopCommand),
    Restart(RestartCommand),
}

#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
/// Start application activity on the target
#[argh(subcommand, name = "start")]
pub struct StartCommand {}

#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
/// Stop application activity on the target
#[argh(subcommand, name = "stop")]
pub struct StopCommand {}

#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
/// Stop application activity on the target and start it again.
#[argh(subcommand, name = "restart")]
pub struct RestartCommand {
    #[argh(option, default = "parse_duration(\"100ms\").unwrap()", from_str_fn(parse_duration))]
    /// the time the system waits before starting application activity again (in nanoseconds).
    /// The system is not guaranteed to start again after this time, but on the next wakeup
    /// this command will take a lease on application activity.
    /// Defaults to 100ms.
    pub wait_time: std::time::Duration,
}
