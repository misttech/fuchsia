// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use assembly_cli_args::{ProductArgs, ValidationMode};
use assembly_config_schema::developer_overrides::DeveloperOverrides;
use assembly_config_schema::{BoardConfig, ProductConfig};
use assembly_container::AssemblyContainer;
use assembly_file_relative_path::SupportsFileRelativePaths;
use assembly_platform_artifacts::PlatformArtifacts;
use assembly_tool::PlatformToolProvider;
use assembly_util::read_config;
use camino::Utf8PathBuf;
use fuchsia_pkg::PackageManifest;
use image_assembly_config_builder::ProductAssembly;
use log::info;

/// Product assembly
pub fn assemble(args: ProductArgs) -> Result<()> {
    let ProductArgs {
        product,
        board_config,
        outdir,
        gendir: _,
        input_bundles_dir,
        package_validation,
        custom_kernel_aib,
        custom_boot_shim_aib,
        suppress_overrides_warning,
        developer_overrides,
        include_example_aib_for_tests,
        mode,
    } = args;

    info!("Reading configuration files.");
    info!("  product: {}", product);

    if package_validation == Some(ValidationMode::WarnOnly) {
        eprintln!(
            "
*=========================================*
* PACKAGE VALIDATION DISABLED FOR PRODUCT *
*=========================================*
Resulting product is not supported and may misbehave!
"
        );
    }

    // Parse the input configs.
    let platform_artifacts = Some(PlatformArtifacts::from_dir_with_path(&input_bundles_dir)?)
        .context("Reading platform artifacts")?;

    #[allow(unused_mut)]
    let mut product_config =
        ProductConfig::from_dir(&product).context("Reading product configuration")?;

    #[cfg(feature = "assembly_heapdump")]
    {
        product_config.platform.development_support.heapdump.component_manager = true;
        product_config.platform.development_support.heapdump.monikers = vec!["/**".to_string()];
    }

    let board_config =
        BoardConfig::from_dir(&board_config).context("Reading board configuration")?;
    let developer_overrides = if let Some(overrides_path) = developer_overrides {
        let developer_overrides = read_config::<DeveloperOverrides>(&overrides_path)
            .context("Reading developer overrides")?
            .resolve_paths_from_file(&overrides_path)
            .context("Resolving paths in developer overrides")?;

        let developer_overrides = developer_overrides.merge_developer_provided_files().context(
            "Merging developer-provided file paths into developer-provided configuration.",
        )?;

        if !suppress_overrides_warning {
            print_developer_overrides_banner(&developer_overrides, &overrides_path)
                .context("Displaying developer overrides.")?;
        }
        Some(developer_overrides)
    } else {
        None
    };

    // Prepare product assembly.
    let mut pa = ProductAssembly::new(
        platform_artifacts,
        product_config,
        board_config,
        include_example_aib_for_tests.unwrap_or(false),
        mode,
        developer_overrides,
    )?;
    if let Some(path) = custom_kernel_aib {
        pa.set_kernel_aib(path);
    }
    if let Some(path) = custom_boot_shim_aib {
        pa.set_boot_shim_aib(path)?;
    }
    if let Some(mode) = package_validation {
        pa.set_validation_mode(mode);
    }

    //////////////////////
    //
    // Generate the output files.  All builder modifications must be complete by here.

    // Serialize the builder state for forensic use.
    let builder_forensics_file_path = outdir.join("assembly_builder_forensics.json");
    let board_forensics_file_path = outdir.join("board_configuration_forensics.json");
    pa.write_forensics_files(builder_forensics_file_path, board_forensics_file_path);

    // Strip the mutability of the builder.
    let pa = pa;

    // Do the actual building and validation of everything for the Image
    // Assembly config.
    let tools = PlatformToolProvider::new(input_bundles_dir);
    let image_assembly_config =
        pa.build(&tools, &outdir).context("Building Image Assembly config")?;

    // Serialize out the Image Assembly configuration.
    let image_assembly_path = outdir.join("image_assembly.json");
    let image_assembly_file = std::fs::File::create(&image_assembly_path).with_context(|| {
        format!("Failed to create image assembly config file: {image_assembly_path}")
    })?;
    serde_json::to_writer_pretty(image_assembly_file, &image_assembly_config)
        .with_context(|| format!("Writing image assembly config file: {image_assembly_path}"))?;

    Ok(())
}

fn print_developer_overrides_banner(
    overrides: &DeveloperOverrides,
    overrides_path: &Utf8PathBuf,
) -> Result<()> {
    let overrides_target = if let Some(target_name) = &overrides.target_name {
        target_name.as_str()
    } else {
        overrides_path.as_str()
    };
    eprintln!();
    eprintln!("WARNING!:  Adding the following via developer overrides from: {overrides_target}");

    let all_packages_in_base = overrides.developer_only_options.all_packages_in_base;
    let netboot_mode = overrides.developer_only_options.netboot_mode;
    if all_packages_in_base || netboot_mode {
        eprintln!();
        eprintln!("  Options:");
        if all_packages_in_base {
            eprintln!("    all_packages_in_base: enabled")
        }
        if netboot_mode {
            eprintln!("    netboot_mode: enabled")
        }
    }

    if overrides.platform.as_object().is_some_and(|p| !p.is_empty()) {
        eprintln!();
        eprintln!("  Platform Configuration Overrides / Additions:");
        for line in serde_json::to_string_pretty(&overrides.platform)?.lines() {
            eprintln!("    {}", line);
        }
    }

    if overrides.product.as_object().is_some_and(|p| !p.is_empty()) {
        eprintln!();
        eprintln!("  Product Configuration Overrides / Additions:");
        for line in serde_json::to_string_pretty(&overrides.product)?.lines() {
            eprintln!("    {}", line);
        }
    }

    if overrides.board.as_object().is_some_and(|p| !p.is_empty()) {
        eprintln!();
        eprintln!("  Board Configuration Overrides / Additions:");
        for line in serde_json::to_string_pretty(&overrides.board)?.lines() {
            eprintln!("    {}", line);
        }
    }

    if !overrides.kernel.command_line_args.is_empty() {
        eprintln!();
        eprintln!("  Additional kernel command line arguments:");
        for arg in &overrides.kernel.command_line_args {
            eprintln!("    {arg}");
        }
    }

    if !overrides.packages.is_empty() {
        eprintln!();
        eprintln!("  Additional packages:");
        for details in &overrides.packages {
            eprintln!("    {} -> {}", details.set, details.package);
        }
    }

    if !overrides.shell_commands.is_empty() {
        eprintln!();
        eprintln!("  Additional shell command stubs:");
        for (entry, components) in &overrides.shell_commands {
            eprintln!("    package: \"{entry}\"");
            for component in components {
                eprintln!("      {component}")
            }
        }
    }

    if !overrides.packages_to_compile.is_empty() {
        eprintln!();
        eprintln!("  Additions to compiled packages:");
        for package in &overrides.packages_to_compile {
            eprintln!("    package: \"{}\"", package.name);
            for component in &package.components {
                eprintln!("      component: \"meta/{}.cm\"", component.component_name);
                for shard in &component.shards {
                    eprintln!("        {shard}");
                }
            }
            if !package.contents.is_empty() {
                eprintln!("      contents:");
                for content in &package.contents {
                    eprintln!("        {}  (from: {})", content.destination, content.source);
                }
            }
        }
    }

    if let Some(path) = &overrides.bootfs_files_package {
        let manifest = PackageManifest::try_load_from(&path)
            .with_context(|| format!("parsing {} as a package manifest", path))?;
        let blobs = manifest.into_blobs();
        if blobs.len() > 1 {
            eprintln!();
            eprintln!("  Additional bootfs files:");
            for blob in blobs {
                if blob.path.starts_with("meta/") {
                    continue;
                }
                eprintln!("    {}  (from: {})", blob.path, blob.source_path);
            }
        }
    }

    eprintln!();
    // And an additional empty line to make sure that any /r's don't attempt to overwrite the last
    // line of this warning.
    eprintln!();
    Ok(())
}
