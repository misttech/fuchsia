// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "gce",
    description = "Start and manage Fuchsia instances directly on Google Compute Engine (GCE).",
    note = "The `gce` command is used to start up, manage, and shut down Fuchsia instances running natively as GCE guest virtual machines without QEMU or nested virtualization."
)]
pub struct GceCommand {
    #[argh(subcommand)]
    pub subcommand: GceSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum GceSubCommand {
    List(ListCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, Default, PartialEq)]
#[argh(subcommand, name = "list", description = "List Fuchsia GCE virtual machine instances.")]
pub struct ListCommand {
    /// GCP Project ID. If unset, uses default from config.
    #[argh(option)]
    pub project: Option<String>,

    /// GCE zone. If unset, uses default from config.
    #[argh(option)]
    pub zone: Option<String>,
}
