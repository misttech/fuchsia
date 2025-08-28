// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use assembled_system::AssembledSystem;
use assembly_artifact_cache::{ArtifactCache, ArtifactError};
use assembly_cli_args::{ProductArgs, ValidationMode};
use assembly_config_schema::{BoardConfig, ProductConfig};
use assembly_container::AssemblyContainer;
use assembly_platform_artifacts::PlatformArtifacts;
use camino::Utf8PathBuf;

pub struct Assembly {
    pub platform_path: Utf8PathBuf,
    pub platform: PlatformArtifacts,
    pub product_config_path: Utf8PathBuf,
    pub product_config: ProductConfig,
    pub board_config_path: Utf8PathBuf,
    pub board_config: BoardConfig,
}

impl Assembly {
    pub fn new(
        cache: &ArtifactCache,
        platform: Option<String>,
        product_config: String,
        board_config: String,
    ) -> Result<Self, ArtifactError> {
        let product_config_path = cache.resolve_product(product_config)?;
        let product_config =
            ProductConfig::from_dir(&product_config_path).context("Reading product config")?;

        let board_config_path = cache.resolve_board(board_config)?;
        let board_config =
            BoardConfig::from_dir(&board_config_path).context("Reading board config")?;

        let platform_path = cache.resolve_platform(platform, &board_config.arch)?;
        let platform =
            PlatformArtifacts::from_dir_with_path(&platform_path).context("Reading platform")?;

        Ok(Self {
            platform_path,
            platform,
            product_config_path,
            product_config,
            board_config_path,
            board_config,
        })
    }

    pub fn version_string(&self) -> String {
        format!(
            "\tplatform: {}@{}
\tproduct_config: {}@{}
\tboard_config: {}@{}",
            self.platform.release_info.name,
            self.platform.release_info.version,
            self.product_config.product.release_info.info.name,
            self.product_config.product.release_info.info.version,
            self.board_config.release_info.info.name,
            self.board_config.release_info.info.version,
        )
    }

    pub async fn create_system(self, outdir: &Utf8PathBuf) -> Result<AssembledSystem> {
        let should_configure_example =
            ffx_config::get::<bool, _>("assembly_example_enabled").unwrap_or_default();
        let gendir = tempfile::TempDir::new().unwrap();
        let gendir = Utf8PathBuf::from_path_buf(gendir.path().to_path_buf()).unwrap();

        let args = ProductArgs {
            product: self.product_config_path,
            board_config: self.board_config_path,
            outdir: outdir.clone(),
            gendir,
            input_bundles_dir: self.platform_path,
            package_validation: Some(ValidationMode::Off),
            custom_kernel_aib: None,
            custom_boot_shim_aib: None,
            suppress_overrides_warning: false,
            developer_overrides: None,
            include_example_aib_for_tests: Some(should_configure_example),
            mode: Default::default(),
        };
        let create_system_outputs = assembly_api::assemble(args)?;
        AssembledSystem::from_dir(create_system_outputs.outdir)
    }
}
