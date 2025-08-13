// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(unused)]

use argh::{ArgsInfo, FromArgs};
use camino::Utf8PathBuf;
use ffx_core::ffx_command;

/// Construct a product bundle using a platform, product config, and board
/// config.
#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "create",
    note = "\
    PLATFORM_ARTIFACT formats:
  <omitted>: in a fuchsia checkout, use locally built platform

  latest: use latest prebuilt platform from CIPD

  29.20250812.4.1: version of a prebuilt platform in
    https://chrome-infra-packages.appspot.com/p/fuchsia/assembly/platform/

  path/to/platform: local path to a platform
    ",
    note = "\
    ARTIFACT formats:
  <name>: in a fuchsia checkout, use locally built artifact by its name

  cipd://<cipd-package>@<cipd-tag>: cipd url for a prebuilt artifact.
    `latest` can be used as <cipd-tag> to fetch the newest artifact.

  path/to/artifact: a path to a locally built artifact
    ",
    example = "\
    // Create a minimal.arm64 product bundle in a local fuchsia checkout.
ffx product-bundle create --product-config minimal --board-config arm64

// This is shorthand for the above command.
ffx product-bundle create minimal.arm64
    ",
    example = "\
    // Create a minimal.arm64 product bundle using prebuilts in CIPD.
ffx product-bundle create
    --platform 29.20250812.5.1
    --product-config cipd://fuchsia/assembly/products/minimal@version:29.20250812.5.1
    --board-config cipd://fuchsia/assembly/boards/arm64@latest
    "
)]
pub struct CreateCommand {
    /// product_config.board_config combination to build when inside a fuchsia
    /// checkout.
    #[argh(positional)]
    pub product_config_board_config_combo: Option<String>,

    /// the platform artifacts to use. See PLATFORM_ARTIFACT below.
    #[argh(option)]
    pub platform: Option<String>,

    /// the product config to use. See ARTIFACT below.
    #[argh(option)]
    pub product_config: Option<String>,

    /// the board config to use. See ARTIFACT below.
    #[argh(option)]
    pub board_config: Option<String>,

    /// the name to add to the output product bundle.
    /// Defaults to product_config.board_config.
    #[argh(option)]
    pub output_name: Option<String>,

    /// the version to add to the output product bundle.
    #[argh(option)]
    pub output_version: Option<String>,

    /// the tuf keys to use.
    #[argh(option)]
    pub tuf_keys: Option<Utf8PathBuf>,

    /// prepare the assembly inputs, but do not run assembly yet.
    #[argh(switch)]
    pub stage: bool,

    /// the location to write the product bundle to.
    #[argh(option)]
    pub out: Option<Utf8PathBuf>,
}
