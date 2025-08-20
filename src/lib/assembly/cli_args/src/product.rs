// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use camino::Utf8PathBuf;
use std::fmt;
use std::str::FromStr;

/// Arguments for performing a high-level product assembly operation.
#[derive(Debug, ArgsInfo, FromArgs, PartialEq)]
#[argh(subcommand, name = "product")]
pub struct ProductArgs {
    /// the product configuration directory.
    #[argh(option)]
    pub product: Utf8PathBuf,

    /// the board configuration directory.
    #[argh(option)]
    pub board_config: Utf8PathBuf,

    /// the directory to write assembled outputs to.
    #[argh(option)]
    pub outdir: Utf8PathBuf,

    /// the directory to write generated intermediate files to.
    #[argh(option)]
    pub gendir: Utf8PathBuf,

    /// the directory in which to find the platform assembly input bundles
    #[argh(option)]
    pub input_bundles_dir: Utf8PathBuf,

    /// disable validation of the assembly's packages
    #[argh(option)]
    pub package_validation: Option<ValidationMode>,

    /// path to an AIB containing a customized kernel zbi to use instead of the
    /// one in the platform AIBs.
    #[argh(option)]
    pub custom_kernel_aib: Option<Utf8PathBuf>,

    /// path to an AIB containing a customized qemu_kernel boot shim to use
    /// instead of the in the platform AIBs.
    #[argh(option)]
    pub custom_boot_shim_aib: Option<Utf8PathBuf>,

    /// whether to hide the warning that shows the overrides that are enabled.
    /// This can be helpful to disable for test assemblies.
    #[argh(switch)]
    pub suppress_overrides_warning: bool,

    /// path to a file specifying developer-level overrides for assembly.
    #[argh(option)]
    pub developer_overrides: Option<Utf8PathBuf>,

    /// flag stating whether the example AIB should be included.
    #[argh(option)]
    pub include_example_aib_for_tests: Option<bool>,
}

impl ProductArgs {
    /// convert args struct to string vector
    pub fn to_vec(&self) -> Vec<String> {
        let mut args = vec![
            "product".to_string(),
            "--product".to_string(),
            self.product.to_string(),
            "--board-config".to_string(),
            self.board_config.to_string(),
            "--outdir".to_string(),
            self.outdir.to_string(),
            "--input-bundles-dir".to_string(),
            self.input_bundles_dir.to_string(),
            "--gendir".to_string(),
            self.gendir.to_string(),
        ];

        if let Some(val) = &self.package_validation {
            args.push("--package-validation".to_string());
            args.push(val.to_string());
        }
        if let Some(path) = &self.custom_kernel_aib {
            args.push("--custom-kernel-aib".to_string());
            args.push(path.to_string());
        }
        if let Some(path) = &self.custom_boot_shim_aib {
            args.push("--custom-boot-shim-aib".to_string());
            args.push(path.to_string());
        }
        if self.suppress_overrides_warning {
            args.push("--suppress-overrides-warning".to_string());
        }
        if let Some(path) = &self.developer_overrides {
            args.push("--developer-overrides".to_string());
            args.push(path.to_string());
        }

        if ffx_config::get::<bool, _>("assembly_example_enabled").unwrap_or_default() {
            args.push("--include-example-aib-for-tests".to_string());
            args.push(true.to_string());
        }
        args
    }
}

/// How to validate the product.
#[derive(Debug, Default, PartialEq, Clone, Copy)]
pub enum ValidationMode {
    /// Do not validate.
    Off,

    /// Validate everything, but print warnings instead of exiting.
    WarnOnly,

    /// Validate everything.
    #[default]
    On,
}

impl fmt::Display for ValidationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationMode::Off => write!(f, "off"),
            ValidationMode::WarnOnly => write!(f, "warning"),
            ValidationMode::On => write!(f, "error"),
        }
    }
}

impl FromStr for ValidationMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "off" => Ok(ValidationMode::Off),
            "warning" => Ok(ValidationMode::WarnOnly),
            "error" => Ok(ValidationMode::On),
            _ => Err(format!(
                "Unknown handling for package validation, valid values are 'off', 'warning' and 'error' (the default): {}",
                s
            )),
        }
    }
}

/// outputs for product assembly operation
pub struct ProductAssemblyOutputs {
    /// path to platform artifacts
    pub platform: Utf8PathBuf,
    /// path to output directory
    pub outdir: Utf8PathBuf,
    /// path to gen directory
    pub gendir: Utf8PathBuf,
    /// path to image assembly config output file
    pub image_assembly_config: Utf8PathBuf,
}

impl From<ProductArgs> for ProductAssemblyOutputs {
    fn from(args: ProductArgs) -> Self {
        let mut image_assembly_config = args.outdir.clone();
        image_assembly_config.push("image_assembly.json");

        ProductAssemblyOutputs {
            platform: args.input_bundles_dir,
            outdir: args.outdir,
            gendir: args.gendir,
            image_assembly_config,
        }
    }
}
