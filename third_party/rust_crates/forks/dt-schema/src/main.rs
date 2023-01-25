// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Error};
use argh::FromArgs;
use logging::LoggingMetadata;
use path::JsonPath;
use tracing::metadata::LevelFilter;
use tracing_subscriber::{prelude::*, Layer, Registry};

mod devicetree;
mod logging;
mod parallel;
mod path;
#[cfg(test)]
mod test_util;
mod validator;

/// STRICT_MODE determines whether or not validation occurs strictly.
/// This MUST NOT be modified outside of |main()|.
static mut STRICT_MODE: bool = true;

pub fn strict_mode() -> bool {
    // Safe because we only set strict mode at the start of program execution.
    unsafe { STRICT_MODE }
}

#[derive(FromArgs)]
/// devicetree schema validation tool.
struct Args {
    #[argh(subcommand)]
    command: Subcommand,

    #[argh(switch, short = 'r')]
    /// perform relaxed validation of devicetree.
    /// this is needed for compatibility with some Linux dtbs.
    relaxed: bool,

    #[argh(switch, short = 'v')]
    /// be verbose.
    verbose: bool,

    #[argh(option)]
    /// list of prefixes to output log messages from. By default all log messages will be output.
    log_filter: Vec<String>,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum Subcommand {
    Validate(ValidateArgs),
    DumpDtb(DumpDtbArgs),
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "validate")]
/// Compile many devicetree schemas into one.
struct ValidateArgs {
    #[argh(option, short = 'd')]
    /// device tree blob file
    dtb: String,
    #[argh(positional)]
    /// files to compile
    schemas: Vec<String>,

    #[argh(option)]
    /// for debugging, path to file to dump generated types to.
    debug_dump_types: Option<String>,

    #[argh(option)]
    /// for debugging, path to jump json DTB to.
    debug_dump_dtb: Option<String>,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "dump")]
/// Dump device tree.
struct DumpDtbArgs {
    #[argh(positional)]
    /// device tree blob file
    dtb: String,
}

fn main() {
    let args: Args = argh::from_env();

    if args.relaxed {
        unsafe {
            // Safe because there is no other code running yet.
            STRICT_MODE = false;
        }
    }

    let filter_layer = LoggingMetadata::new(args.log_filter);

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_filter(if args.verbose {
            LevelFilter::TRACE
        } else {
            LevelFilter::INFO
        })
        .boxed();
    let subscriber = Registry::default().with(fmt_layer).with(filter_layer);

    tracing::subscriber::set_global_default(subscriber).unwrap();
    match args.command {
        Subcommand::Validate(validate) => match do_validate(validate) {
            Ok(()) => println!("Success."),
            Err(e) => println!("Error: {:?}", e),
        },
        Subcommand::DumpDtb(args) => println!(
            "{:?}",
            devicetree::Devicetree::from_reader(std::fs::File::open(args.dtb).unwrap())
        ),
    };
}

fn validate_subtree(
    start: &serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
    validator: &validator::Validator,
) -> Result<(), BTreeSet<JsonPath>> {
    let failed = !validator.validate(&start.clone().into(), path.clone());
    let mut errors = BTreeSet::new();
    for (k, v) in start
        .iter()
        .filter_map(|(k, v)| v.as_object().map(|v| (k, v)))
    {
        match validate_subtree(v, path.extend(k), validator) {
            Ok(()) => {}
            Err(new_errors) => {
                errors.extend(new_errors.into_iter());
            }
        }
    }

    if failed {
        errors.insert(path);
        Err(errors)
    } else if !errors.is_empty() {
        Err(errors)
    } else {
        Ok(())
    }
}

fn do_validate(args: ValidateArgs) -> Result<(), Error> {
    let validator =
        validator::Validator::new_with_schemas(&args.schemas).context("Loading schemas")?;

    if let Some(dest) = args.debug_dump_types {
        let out = std::fs::File::create(dest).context("Opening file")?;
        validator
            .dump_properties(out)
            .context("dumping properties")?;
    }

    let devicetree = devicetree::Devicetree::from_reader(
        std::fs::File::open(args.dtb.clone())
            .with_context(|| format!("Opening file {}", args.dtb))?,
    )
    .context("parsing device tree blob")?;

    let json = devicetree.as_json(&validator)?;

    if let Some(dest) = args.debug_dump_dtb {
        let out = std::fs::File::create(dest).context("Creating dtb debug file")?;
        serde_json::to_writer_pretty(out, &json).context("Writing dtb debug file")?;
    }

    match validate_subtree(json.as_object().unwrap(), JsonPath::new(), &validator) {
        Ok(()) => Ok(()),
        Err(error_locations) => {
            let mut paths_string: Vec<String> =
                error_locations.into_iter().map(|s| s.to_string()).collect();
            paths_string.sort();
            tracing::error!(
                "Errors were encountered in the following paths: '{}'",
                paths_string.join("', '")
            );

            Err(anyhow!("Failed to validate device tree"))
        }
    }
}
