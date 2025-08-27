// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod assembly;

use anyhow::{Context, Result, anyhow};
use assembly::Assembly;
use assembly_artifact_cache::{ArtifactCache, ArtifactError};
use assembly_tool::PlatformToolProvider;
use camino::Utf8PathBuf;
use delivery_blob::DeliveryBlobType;
use errors::FfxError;
use ffx_config::EnvironmentContext;
use ffx_product_create_args::CreateCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool};
use product_bundle::{ProductBundleBuilder, Slot};
use tempfile::tempdir;

#[derive(FfxTool)]
pub struct ProductBundleCreateTool {
    #[command]
    pub cmd: CreateCommand,
    ctx: EnvironmentContext,
}

fho::embedded_plugin!(ProductBundleCreateTool);

/// Create a fuchsia product bundle.
#[async_trait::async_trait(?Send)]
impl FfxMain for ProductBundleCreateTool {
    type Writer = SimpleWriter;
    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        let build_dir = self
            .ctx
            .build_dir()
            .map(|p| {
                Utf8PathBuf::from_path_buf(p.to_path_buf()).map_err(|build_dir| {
                    fho::bug!("Failed to parse build_dir as utf8: {}", build_dir.display())
                })
            })
            .transpose()?;
        product_bundle_create(self.cmd, build_dir, writer).await.map_err(flatten_error_sources)
    }
}

/// Create a fuchsia product bundle and return an anyhow result.
/// This allows us to work with anyhow, and map to a fho result above.
async fn product_bundle_create(
    cmd: CreateCommand,
    build_dir: Option<Utf8PathBuf>,
    writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let sanitized_cmd = cmd.try_into()?;
    Box::pin(sanitized_product_bundle_create(sanitized_cmd, build_dir, writer)).await
}

/// Convert the anyhow error into a pretty/stacked fho error.
fn flatten_error_sources(e: ArtifactError) -> fho::Error {
    let suggestion =
        e.suggestion.map(|s| format!("\n\nSuggestion: {}", s)).unwrap_or_else(|| "".to_string());
    FfxError::Error(
        anyhow::anyhow!(
            "Failed: {}{}{}",
            e.error,
            e.error
                .chain()
                .skip(1)
                .enumerate()
                .map(|(i, e)| format!("\n  {: >3}.  {}", i + 1, e))
                .collect::<Vec<String>>()
                .concat(),
            suggestion,
        ),
        -1,
    )
    .into()
}

/// All the inputs necessary to run `ffx product-bundle create` after checking
/// that the arguments were properly given.
struct SanitizedCreateCommand {
    /// The platform artifacts to use.
    /// If None, then we use the default local artifacts.
    pub platform: Option<String>,

    /// The product config to use.
    pub product_config: String,

    /// The board config to use.
    pub board_config: String,

    /// The name to add to the output product bundle.
    pub output_name: Option<String>,

    /// The version to add to the output product bundle.
    pub output_version: Option<String>,

    /// The tuf keys to use.
    pub tuf_keys: Option<Utf8PathBuf>,

    /// What result we want from running `ffx product create`.
    pub result: CreateResult,
}

/// What result we want from running `ffx product create`.
enum CreateResult {
    /// Stage the inputs, but do not run assembly.
    Stage,

    /// Run assembly and generate the outputs to this path.
    Out(Utf8PathBuf),

    /// Run assembly and generates the outputs to the default location.
    Default,
}

/// Parse the input command, and ensure that the user formatted it correctly,
/// then return a new command with all the requirements for running
/// `ffx product create`.
impl TryFrom<CreateCommand> for SanitizedCreateCommand {
    type Error = anyhow::Error;

    fn try_from(cmd: CreateCommand) -> Result<Self> {
        let platform = cmd.platform;

        // Determine what result we want from this command.
        let result = match (cmd.stage, cmd.out) {
            (true, Some(_)) => {
                anyhow::bail!("--stage and --out cannot be used together.")
            }
            (true, None) => CreateResult::Stage,
            (false, Some(out)) => CreateResult::Out(out),
            (false, None) => CreateResult::Default,
        };

        // Choose between a product_config.board_config combo and --product --board flags.
        let (product_config, board_config) =
            if let Some(combo) = cmd.product_config_board_config_combo {
                let (p, b) = combo
                    .split_once(".")
                    .context("product_config.board_config combo must have a period")?;
                (p.to_string(), b.to_string())
            } else {
                let p = cmd.product_config.context("--product-config is missing")?;
                let b = cmd.board_config.context("--board-config is missing")?;
                (p, b)
            };

        let output_name = cmd.output_name;
        let output_version = cmd.output_version;
        let tuf_keys = cmd.tuf_keys;
        Ok(Self {
            platform,
            product_config,
            board_config,
            output_name,
            output_version,
            tuf_keys,
            result,
        })
    }
}

fn default_path_for_product_bundle_name(name: impl AsRef<str>) -> Result<Utf8PathBuf> {
    let home = std::env::home_dir().context("Getting the home dir")?;
    let home = Utf8PathBuf::from_path_buf(home)
        .map_err(|e| anyhow!("error converting path to utf8: {:?}", e))?;
    Ok(home.join(".fuchsia").join("product_bundles").join(name.as_ref()))
}

/// Construct a product bundle using sanitized inputs.
async fn sanitized_product_bundle_create(
    cmd: SanitizedCreateCommand,
    build_dir: Option<Utf8PathBuf>,
    mut writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let tmp = tempdir().unwrap();
    let tmp_path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    let cache = ArtifactCache::new(build_dir)?;
    let assembly = Assembly::new(&cache, cmd.platform, cmd.product_config, cmd.board_config)?;
    writer
        .line(format!("Staged the artifacts\n{}", assembly.version_string()))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    let name = cmd.output_name.unwrap_or_else(|| {
        let product_name = assembly.product_config.product.release_info.info.name.clone();
        let board_name = assembly.board_config.release_info.info.name.clone();
        format!("{}.{}", product_name, board_name)
    });

    // Return early if we are only staging the inputs.
    let out = match cmd.result {
        CreateResult::Stage => return Ok(()),
        CreateResult::Out(out) => Ok(out),
        CreateResult::Default => default_path_for_product_bundle_name(&name),
    }?;

    let version = cmd
        .output_version
        .unwrap_or_else(|| assembly.product_config.product.release_info.info.version.clone());
    let update_version_file = tmp_path.join("update_version.txt");
    std::fs::write(&update_version_file, &version)
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    writer
        .line(format!("Assembling into {} ...", &out))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;
    let tools = PlatformToolProvider::new(assembly.platform_path.clone());
    let system = Box::pin(assembly.create_system(&tmp_path.join("system"))).await?;
    let mut builder = ProductBundleBuilder::new(name, version)
        .system(system, Slot::A)
        .update_package(update_version_file, 1);

    if let Some(tuf_keys) = cmd.tuf_keys {
        builder = builder.repository(DeliveryBlobType::Type1, tuf_keys);
    }
    let _ = builder.build(Box::new(tools), &out).await?;
    cache.purge()?;

    println!(
        "Next, try flashing this product bundle:\n\tffx target flash -b {}",
        shorten_path(&out)
    );
    Ok(())
}

/// Shorten a path if possible, by trying to make it relative to home or the
/// current working directory.
fn shorten_path(path: &Utf8PathBuf) -> String {
    let mut path_str = path.to_string();

    // Try to replace the home directory with ~.
    if let Some(home) = std::env::home_dir() {
        if let Ok(home) = Utf8PathBuf::from_path_buf(home) {
            if let Ok(stripped) = path.strip_prefix(&home) {
                let new_path = format!("~/{}", stripped);
                if new_path.len() < path_str.len() {
                    path_str = new_path;
                }
            }
        }
    }

    // Try to make the path relative to the current working directory.
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(rel) = pathdiff::diff_paths(path, cwd) {
            let new_path = rel.to_string_lossy().to_string();
            if new_path.len() < path_str.len() {
                path_str = new_path;
            }
        }
    }
    path_str
}

#[cfg(test)]
mod test {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    // This test mucks with $HOME, therefore we run it serially to make sure it
    // does not affect the other tests. This is necessary because some test
    // environments set $HOME to /tmp.
    #[test]
    #[serial]
    fn test_shorten_path() {
        let tmp = tempdir().unwrap();
        let tmp_path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        // Set $HOME to /tmp/home, while saving the previous $HOME.
        let mock_home = tmp_path.join("home");
        std::fs::create_dir(&mock_home).unwrap();
        let original_home = std::env::var("HOME").unwrap();
        std::env::set_var("HOME", &mock_home);

        // A path in the home directory.
        let path = mock_home.join("foo");
        assert_eq!(shorten_path(&path), "~/foo");

        // A path in the current directory.
        let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
        let path = cwd.join("foo");
        assert_eq!(shorten_path(&path), "foo");

        // A path outside both home and CWD.
        let path = tmp_path.join("foo");
        assert_eq!(shorten_path(&path), path.to_string());

        // Restore $HOME.
        std::env::set_var("HOME", original_home);
    }
}
