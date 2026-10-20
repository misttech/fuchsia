// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use argh::FromArgs;
use camino::Utf8PathBuf;
use fidl_fuchsia_images2_common::{PixelFormat, PixelFormatModifier};
use fidl_fuchsia_sysmem2_common::{BufferUsage, FormatCostEntry, FormatCostKey, FormatCosts};

#[derive(FromArgs)]
/// Produce the sysmem format_costs persistent FIDL file for PowerVR PVRIC.
struct Args {
    /// path to output file
    #[argh(option)]
    output: Utf8PathBuf,
}

fn generate_pvric_format_costs(modifier: PixelFormatModifier, cost: f32) -> Vec<FormatCostEntry> {
    vec![
        PixelFormat::B8G8R8A8,
        PixelFormat::R8G8B8A8,
        PixelFormat::R8G8B8X8,
        PixelFormat::B8G8R8X8,
        PixelFormat::A2R10G10B10,
        PixelFormat::A2B10G10R10,
        PixelFormat::R5G6B5,
    ]
    .into_iter()
    .map(|pixel_format| FormatCostEntry {
        key: Some(FormatCostKey {
            pixel_format: Some(pixel_format),
            pixel_format_modifier: Some(modifier),
            buffer_usage_bits: Some(BufferUsage::default()),
            ..Default::default()
        }),
        cost: Some(cost),
        ..Default::default()
    })
    .collect()
}

fn generate_format_cost_entries() -> Vec<FormatCostEntry> {
    struct ModifierAndCost {
        modifier: PixelFormatModifier,
        cost: f32,
    }
    // 8x8 tile layout is optimal for GPU-only textures/targets when display scanout is not
    // involved; 16x4 tile layout is preferred when both GPU and display scanout participate.
    // Both have lower cost than kLinear (DBL_MAX).
    vec![
        ModifierAndCost { modifier: PixelFormatModifier::PvrFbcdc8X8V14, cost: 100.0 },
        ModifierAndCost { modifier: PixelFormatModifier::PvrFbcdc16X4V14, cost: 200.0 },
    ]
    .into_iter()
    .fold(vec![], |mut acc, x| {
        acc.append(&mut generate_pvric_format_costs(x.modifier, x.cost));
        acc
    })
}

fn generate_and_write_format_costs(output_filename: &Utf8PathBuf) -> Result<(), Error> {
    let format_costs =
        FormatCosts { format_costs: Some(generate_format_cost_entries()), ..Default::default() };
    let format_costs_persistent_fidl_vec = fidl::persist(&format_costs).context("fidl::Persist")?;
    std::fs::write(output_filename, &format_costs_persistent_fidl_vec).context("std::fs::write")?;
    Ok(())
}

fn main() {
    let args: Args = argh::from_env();
    generate_and_write_format_costs(&args.output).expect("generate_and_write_format_costs");
}
