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
    LongestDimension,
    /// Bisect all dimensions in the search space simultaneously.
    #[default]
    AllDimensions,
}

impl FromArgValue for Strategy {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value {
            "longest-dimension" => Ok(Strategy::LongestDimension),
            "all-dimensions" => Ok(Strategy::AllDimensions),
            _ => Err(format!(
                "invalid strategy: {value}. Expected 'longest-dimension' or 'all-dimensions'"
            )),
        }
    }
}

/// Generate a list of released versions between [--from-success] and [--to-failure] for every
/// artifact contained within the given product bundle (e.g. platform, product, board).
///
/// Assemble intermediate product bundles using combinations of the artifacts, and facilitate
/// running tests on those PBs to quickly identify which artifact is the source of a bug or
/// behavior change.
#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "bisect",
    note = "\
    VERSION formats:
  This parameter type (from-success, to-failure) refers to the product bundle version.

  Steps to retrieve this information depends on which product bundle is being bisected,
  but it can always be retrieved from the product bundle itself by running:

    ffx product-bundle get-version <path/to/product/bundle>
    ",
    note = "\
    For more information about how to use this tool, see go/fuchsia-product-bisection-userguide
    ",
    example = "\
    // Bisect the core.vim3 product bundle between two provided versions.
// (Note: core.vim3 is not yet supported: https://fxbug.dev/485981469 .)

ffx product-bundle bisect core.vim3 \\
    --from-success 29.20250826.6.1 \\
    --to-failure 29.20250905.6.1
    "
)]
pub struct BisectCommand {
    /// name of the product bundle being bisected.
    #[argh(positional)]
    pub name: String,

    /// known-good version of the product bundle. The bisection search window begins here.
    #[argh(option)]
    pub from_success: String,

    /// known-bad version of the product bundle. The bisection search window ends here.
    #[argh(option)]
    pub to_failure: String,

    /// slot to bisect over (a or r). Defaults to slot A.
    #[argh(option, default = "Default::default()")]
    pub slot: assembly_artifact_cache::Slot,

    /// search strategy to use. Defaults to "AllDimensions", which bisects across all pb artifact
    /// types (platform, product, board...) simultaneously.
    #[argh(option, default = "Strategy::default()")]
    pub strategy: Strategy,

    /// directory to write assembled images and other artifacts. Defaults to ~/<plan_directory>/out
    #[argh(option)]
    pub out_dir: Option<Utf8PathBuf>,

    /// directory to write intermediate files. Defaults to ~/<plan_directory>/gen
    #[argh(option)]
    pub gen_dir: Option<Utf8PathBuf>,

    /// authentication method to use.
    #[argh(option, default = "AuthFlowChoice::Default")]
    pub auth: AuthFlowChoice,
}
