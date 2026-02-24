// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "peer",
    description = "Show details for a known peer.",
    example = "ffx bluetooth peer"
)]
pub struct PeerCommand {
    #[argh(subcommand)]
    pub subcommand: PeerSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum PeerSubCommand {
    List(ListCommand),
    Show(ShowCommand),
    Connect(ConnectCommand),
    Disconnect(DisconnectCommand),
    Forget(ForgetCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "list",
    description = "Show all known peers in a summarized view (optionally filtered).",
    example = "ffx bluetooth peer list <filter>"
)]
pub struct ListCommand {
    /// filter all known peers by id, address, or name (case-insensitive)
    #[argh(positional)]
    pub filter: Option<String>,

    /// show details for all known peers
    #[argh(switch)]
    pub details: bool,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "show",
    description = "Show details for a known peer.",
    example = "ffx bluetooth peer show <id|addr>"
)]
pub struct ShowCommand {
    #[argh(positional)]
    pub id_or_addr: ffx_bluetooth_common::PeerIdOrAddr,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "connect",
    description = "Connect to a peer.",
    example = "ffx bluetooth peer connect <id|addr>"
)]
pub struct ConnectCommand {
    #[argh(positional)]
    pub id_or_addr: ffx_bluetooth_common::PeerIdOrAddr,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "disconnect",
    description = "Disconnect from a peer.",
    example = "ffx bluetooth peer disconnect <id|addr>"
)]
pub struct DisconnectCommand {
    #[argh(positional)]
    pub id_or_addr: ffx_bluetooth_common::PeerIdOrAddr,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "forget",
    description = "Delete and disconnect a peer.",
    example = "ffx bluetooth peer forget <id|addr>"
)]
pub struct ForgetCommand {
    #[argh(positional)]
    pub id_or_addr: ffx_bluetooth_common::PeerIdOrAddr,
}
