// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgValue, FromArgs};
use camino::Utf8PathBuf;
use ffx_core::ffx_command;
use pbms::AuthFlowChoice;

/// The search strategy to use for bisection.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum Strategy {
    /// Bisect the longest dimension in the search space.
    #[default]
    LongestDimension,
    /// Bisect all dimensions in the search space simultaneously.
    AllDimensions,
}

impl FromArgValue for Strategy {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "longest_dimension" => Ok(Self::LongestDimension),
            "all_dimensions" => Ok(Self::AllDimensions),
            _ => Err(format!("unknown strategy '{}'", value)),
        }
    }
}

/// Arguments to run bisection.
#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand, name = "bisect")]
pub struct BisectCommand {
    /// product bundle name to bisect.
    #[argh(option)]
    pub name: String,

    /// latest known-good version of the product bundle.
    #[argh(option)]
    pub from_success: String,

    /// earliest known-bad version of the product bundle.
    #[argh(option)]
    pub to_failure: String,

    /// directory to write assembled images and other artifacts.
    #[argh(option)]
    pub out_dir: Option<Utf8PathBuf>,

    /// directory to write intermediate files.
    #[argh(option)]
    pub gen_dir: Option<Utf8PathBuf>,

    /// authentication method to use.
    #[argh(option, default = "AuthFlowChoice::Default")]
    pub auth: AuthFlowChoice,

    /// search strategy to use.
    #[argh(option, default = "Strategy::default()")]
    pub strategy: Strategy,
}
