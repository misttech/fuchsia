// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use argh::FromArgs;
use std::path::PathBuf;

/// A tool to interact with Fxfs images within product bundles.
#[derive(FromArgs, Debug)]
struct FxfsToolArgs {
    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Extract(ExtractArgs),
}

/// Extracts blobs from an Fxfs image.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "extract")]
struct ExtractArgs {
    /// path to the source fxfs sparse image
    #[argh(option)]
    image: PathBuf,

    /// path to the destination directory where extracted blobs will be written
    #[argh(option)]
    out: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args: FxfsToolArgs = argh::from_env();

    match args.subcommand {
        SubCommand::Extract(extract_args) => {
            println!("Extracting from: {:?} to: {:?}", extract_args.image, extract_args.out);
            // TODO b/472511115: Implement extraction logic here
        }
    }
}
