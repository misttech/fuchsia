// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A ffx plugin for bisecting product bundles.

#![deny(missing_docs)]

use crate::bisection_controller::BisectionController;
use anyhow::{Context, Result};
use assembly_artifact_cache::{ArtifactCache, MOSClient};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ffx_config::EnvironmentContext;
use ffx_product_bundle_bisect_args::BisectCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool};
use gcs::client::Client as GcsClient;
use pbms::handle_new_access_token;
use std::fs;
use {
    assembly_config_schema as _, assembly_container as _, assembly_platform_artifacts as _,
    structured_ui,
};

mod bisection_controller;
mod bisection_plan;
mod search_space;
mod strategies;
mod versioned_artifact_set;

/// The ffx tool for bisecting product bundles.
#[derive(FfxTool)]
pub struct ProductBisectTool {
    /// The command line arguments for the bisect tool.
    #[command]
    pub cmd: BisectCommand,

    /// The ffx environment context.
    env_context: EnvironmentContext,
}

fho::embedded_plugin!(ProductBisectTool);

use gcs::error::GcsError;

/// Initiate PB Bisection.
#[async_trait(?Send)]
impl FfxMain for ProductBisectTool {
    type Writer = SimpleWriter;
    /// Main entry point for the `ffx product bisect` tool.
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        writer.line("")?;

        let mut controller = match setup(self.cmd.clone(), &mut writer, &self.env_context).await {
            Ok(controller) => controller,
            Err(e) => {
                if let Some(GcsError::NeedNewRefreshToken) = e.downcast_ref::<GcsError>() {
                    writer.line("Authentication expired. Please run `ffx auth login`.")?;
                }
                return Err(e.into());
            }
        };
        if let Err(e) = controller.run().await {
            if let Some(GcsError::NeedNewRefreshToken) = e.downcast_ref::<GcsError>() {
                writer.line("Authentication expired. Please run `ffx auth login`.")?;
            }
            return Err(e.into());
        }
        Ok(())
    }
}

async fn setup<'a>(
    cmd: BisectCommand,
    writer: &'a mut SimpleWriter,
    env_context: &'a EnvironmentContext,
) -> Result<BisectionController<'a>> {
    // Create a directory for storing plan status and search results.
    let home = std::env::home_dir().context("Could not find home directory")?;
    let fuchsia_home = Utf8PathBuf::from_path_buf(home.join(".fuchsia"))
        .map_err(|p| anyhow::anyhow!("Path is not valid UTF-8: {}", p.display()))?;
    let bisect_home = fuchsia_home.join("bisect");
    let plan_home =
        bisect_home.join(&cmd.name).join(format!("{}_to_{}", cmd.from_success, cmd.to_failure));
    fs::create_dir_all(&plan_home)
        .with_context(|| format!("Failed to create plan directory at {}", plan_home))?;

    // Create a MOS client. Reuse the setup logic from the GCS library.
    let gcs_client = GcsClient::initial().context("Failed to initialize GCS client")?;
    let token = handle_new_access_token(&cmd.auth, &structured_ui::MockUi::new())
        .await
        .context("Failed to get new access token")?;
    gcs_client.set_access_token(token).await;
    let client = MOSClient::new(gcs_client.clone());

    // Create an artifact cache for reusing assembly artifacts.
    let build_dir = env_context
        .build_dir()
        .map(|p| {
            Utf8PathBuf::from_path_buf(p.to_path_buf()).map_err(|build_dir| {
                anyhow::anyhow!("Failed to parse build_dir as utf8: {}", build_dir.display())
            })
        })
        .transpose()?;
    let cache = ArtifactCache::new(build_dir, gcs_client.clone())
        .context("Failed to create artifact cache")?;

    // Create the bisection controller.
    let mut controller =
        BisectionController::new(cmd, plan_home, client, cache, writer, env_context).await?;
    controller.save()?;

    Ok(controller)
}
