// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;
use ffx_repository_serve::DEFAULT_REPO_NAME;
use std::path::PathBuf;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, PartialEq, Debug)]
#[argh(
    subcommand,
    name = "add-from-pm",
    description = "Make the daemon aware of a specific pm-built repository"
)]
pub struct AddFromPmCommand {
    // LINT.IfChange
    /// repositories will be named `NAME`. Defaults to `devhost`.
    #[argh(option, short = 'r', default = "DEFAULT_REPO_NAME.into()")]
    pub repository: String,
    // LINT.ThenChange(../../serve/src/lib.rs)
    /// alias this repository to these names when this repository is registered on a target.
    #[argh(option, long = "alias")]
    pub aliases: Vec<String>,

    /// path to the pm-built package repository.
    #[argh(positional)]
    pub pm_repo_path: PathBuf,
}
