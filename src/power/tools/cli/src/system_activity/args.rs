// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::application_activity::args::ApplicationActivityCommand;
use argh::{ArgsInfo, FromArgs};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "system-activity", description = "Manipulate SAG power elements")]
pub struct SystemActivityCommand {
    #[argh(subcommand)]
    pub subcommand: SystemActivitySubcommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum SystemActivitySubcommand {
    ApplicationActivity(ApplicationActivityCommand),
}
