// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

// ffx bluetooth controller
#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "controller",
    description = "Interact with the Bluetooth controller(s) available to the system.",
    example = "ffx bluetooth controller"
)]
pub struct ControllerCommand {
    #[argh(subcommand)]
    pub subcommand: ControllerSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum ControllerSubCommand {
    Show(ShowCommand),
    List(ListCommand),
}
/// ffx bluetooth controller show
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "show",
    description = "Show details about the active Bluetooth controller.",
    example = "ffx bluetooth controller show"
)]
pub struct ShowCommand {}

/// ffx bluetooth controller list
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "list",
    description = "List information about all Bluetooth controllers available to the system.",
    example = "ffx bluetooth controller list"
)]
pub struct ListCommand {}
