// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use assembled_system::AssembledSystem;
use assembly_api::release_info::*;
use assembly_artifact_cache::{ArtifactCache, ArtifactError};
use assembly_cli_args::{ProductArgs, ValidationMode};
use assembly_container::AssemblyContainer;
use assembly_release_info::{BoardReleaseInfo, ProductReleaseInfo, ReleaseInfo};
use camino::Utf8PathBuf;
use ffx_config::EnvironmentContext;

pub struct Assembly {
    pub platform_path: Utf8PathBuf,
    pub platform_release_info: ReleaseInfo,
    pub product_config_path: Utf8PathBuf,
    pub product_config_release_info: ProductReleaseInfo,
    pub board_config_path: Utf8PathBuf,
    pub board_config_release_info: BoardReleaseInfo,
}

impl Assembly {
    pub async fn new(
        cache: &ArtifactCache,
        platform: Option<String>,
        product_config: String,
        board_config: String,
    ) -> Result<Self, ArtifactError> {
        let product_config_path = cache.resolve_product(product_config).await?;
        let product_config_release_info = load_product_release_info(&product_config_path)?;

        let board_config_path = cache.resolve_board(board_config).await?;
        let board_config_release_info = load_board_release_info(&board_config_path)?;
        let arch: assembly_config_schema::board_config::Architecture =
            load_board_arch(&board_config_path)?.parse()?;

        let platform_path = cache.resolve_platform(platform, &arch).await?;
        let platform_release_info = load_platform_release_info(&platform_path)?;

        Ok(Self {
            platform_path,
            platform_release_info,
            product_config_path,
            product_config_release_info,
            board_config_path,
            board_config_release_info,
        })
    }

    pub fn version_string(&self) -> String {
        format!(
            "\tplatform: {}@{}\n\tproduct_config: {}@{}\n\tboard_config: {}@{}",
            self.platform_release_info.name,
            self.platform_release_info.version,
            self.product_config_release_info.info.name,
            self.product_config_release_info.info.version,
            self.board_config_release_info.info.name,
            self.board_config_release_info.info.version,
        )
    }

    pub async fn create_system(
        self,
        context: &EnvironmentContext,
        should_configure_example: bool,
        outdir: &Utf8PathBuf,
    ) -> Result<AssembledSystem> {
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
        let create_system_outputs = assembly_api::assemble(context, args)?;
        AssembledSystem::from_dir(create_system_outputs.outdir)
    }
}
