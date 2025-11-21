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
use ffx_product_bundle_create_args::CreateCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool};
use product_bundle::{ProductBundleBuilder, Slot};
use tempfile::tempdir;
use {gcs, pbms, structured_ui};

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
        product_bundle_create(&self.ctx, self.cmd, build_dir, writer)
            .await
            .map_err(flatten_error_sources)
    }
}

/// Create a fuchsia product bundle and return an anyhow result.
/// This allows us to work with anyhow, and map to a fho result above.
async fn product_bundle_create(
    context: &EnvironmentContext,
    cmd: CreateCommand,
    build_dir: Option<Utf8PathBuf>,
    writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let sanitized_cmd = cmd.try_into()?;
    Box::pin(sanitized_product_bundle_create(context, sanitized_cmd, build_dir, writer)).await
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
#[derive(Debug)]
struct SanitizedCreateCommand {
    /// The platform artifacts to use.
    /// If None, then we use the default local artifacts.
    pub platform: Option<String>,

    /// The product config to use.
    pub product_config: String,

    /// The product config to use for the recovery image.
    pub recovery_product_config: Option<String>,

    /// The board config to use.
    pub board_config: String,

    /// The board config to use for the recovery image.
    pub recovery_board_config: Option<String>,

    /// The name to add to the output product bundle.
    pub output_name: Option<String>,

    /// The version to add to the output product bundle.
    pub output_version: Option<String>,

    /// The path to a file containing the version to add to the output product bundle.
    pub output_version_file: Option<Utf8PathBuf>,

    /// The authentication flow to use to access googleapis.
    pub auth: pbms::AuthFlowChoice,

    /// The tuf keys to use.
    pub tuf_keys: Option<Utf8PathBuf>,

    /// path to the Ed25519 private key in PEM format to sign the ota manifest.
    pub ota_manifest_key: Option<Utf8PathBuf>,

    /// What result we want from running `ffx product create`.
    pub result: CreateResult,
}

/// What result we want from running `ffx product create`.
#[derive(Debug)]
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

        if cmd.output_version.is_some() && cmd.output_version_file.is_some() {
            anyhow::bail!("--output-version and --output-version-file cannot be used together.");
        }

        let CreateCommand {
            output_name,
            output_version,
            output_version_file,
            tuf_keys,
            ota_manifest_key,
            auth,
            recovery_product_config,
            recovery_board_config,
            ..
        } = cmd;

        if recovery_board_config.is_some() && recovery_product_config.is_none() {
            anyhow::bail!(
                "--recovery-product-config is required if --recovery-board-config is specified."
            );
        }

        Ok(Self {
            platform,
            product_config,
            recovery_product_config,
            board_config,
            recovery_board_config,
            output_name,
            output_version,
            output_version_file,
            auth,
            tuf_keys,
            ota_manifest_key,
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
    context: &EnvironmentContext,
    cmd: SanitizedCreateCommand,
    build_dir: Option<Utf8PathBuf>,
    mut writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let tmp = tempdir().unwrap();
    let tmp_path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    let gcs_client = gcs::client::Client::initial().map_err(|e| {
        ArtifactError::new(anyhow::anyhow!("Failed to initialize GCS client: {}", e))
    })?;
    if !matches!(cmd.auth, pbms::AuthFlowChoice::NoAuth) {
        let token = pbms::handle_new_access_token(&cmd.auth, &structured_ui::MockUi::new())
            .await
            .context("Failed to handle or retrieve new access token")?;
        gcs_client.set_access_token(token).await;
    }

    let cache = ArtifactCache::new(build_dir, gcs_client)?;
    let assembly =
        Assembly::new(&cache, cmd.platform.clone(), cmd.product_config, cmd.board_config.clone())
            .await?;
    writer
        .line(format!("Staged the artifacts\n{}", assembly.version_string()))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    let name = cmd.output_name.unwrap_or_else(|| {
        let product_name = assembly.product_config_release_info.info.name.clone();
        let board_name = assembly.board_config_release_info.info.name.clone();
        format!("{}.{}", product_name, board_name)
    });

    // Return early if we are only staging the inputs.
    let out = match cmd.result {
        CreateResult::Stage => return Ok(()),
        CreateResult::Out(out) => Ok(out),
        CreateResult::Default => default_path_for_product_bundle_name(&name),
    }?;

    let version = if let Some(version_file) = cmd.output_version_file {
        read_version_from_file(version_file)?
    } else {
        cmd.output_version
            .unwrap_or_else(|| assembly.product_config_release_info.info.version.clone())
    };
    let update_version_file = tmp_path.join("update_version.txt");
    std::fs::write(&update_version_file, &version)
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    writer
        .line(format!("Assembling into {} ...", &out))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;
    let tools = PlatformToolProvider::new(assembly.platform_path.clone());
    let should_configure_example =
        context.get::<bool, _>("assembly_example_enabled").unwrap_or_default();
    let system = Box::pin(assembly.create_system(
        context,
        should_configure_example,
        &tmp_path.join("system"),
    ))
    .await?;
    let mut builder = ProductBundleBuilder::new(name.clone(), version)
        .system(system, Slot::A)
        .update_package(update_version_file, 1, cmd.ota_manifest_key);

    if let Some(tuf_keys) = cmd.tuf_keys {
        builder = builder.repository(DeliveryBlobType::Type1, tuf_keys);
    }

    // Build the recovery image if requested.
    if let Some(recovery_product_config) = cmd.recovery_product_config {
        let recovery_assembly = Assembly::new(
            &cache,
            cmd.platform.clone(),
            recovery_product_config,
            cmd.recovery_board_config.unwrap_or_else(|| cmd.board_config.clone()),
        )
        .await?;
        let recovery_system = Box::pin(recovery_assembly.create_system(
            context,
            should_configure_example,
            &tmp_path.join("recovery_system"),
        ))
        .await?;
        builder = builder.system(recovery_system, Slot::R);
    }

    let _ = builder.build(Box::new(tools), &out).await?;
    cache.purge()?;

    println!(
        "\nNext, try flashing this product bundle:\n\tffx target flash -b {}",
        shorten_path(&out)
    );
    println!(
        "\nOr archive the product bundle to share it with someone else:\
         \n\t(cd {} && zip -r ~/{}.zip *)",
        shorten_path(&out.into()),
        name.clone(),
    );

    Ok(())
}

/// Read the product version from a file.
fn read_version_from_file(version_file: Utf8PathBuf) -> Result<String> {
    Ok(std::fs::read_to_string(&version_file)
        .with_context(|| format!("Failed to read version file '{}'", version_file))?
        .trim()
        .to_string())
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
    use pbms::AuthFlowChoice;
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
        unsafe { std::env::set_var("HOME", &mock_home) };

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
        unsafe { std::env::set_var("HOME", original_home) };
    }

    #[test]
    fn test_recovery_board_config_requires_recovery_product_config() {
        let cmd = CreateCommand {
            product_config_board_config_combo: None,
            platform: None,
            product_config: Some("product".to_string()),
            recovery_product_config: None,
            board_config: Some("board".to_string()),
            recovery_board_config: Some("recovery_board".to_string()),
            output_name: None,
            output_version: None,
            output_version_file: None,
            tuf_keys: None,
            ota_manifest_key: None,
            stage: false,
            out: None,
            auth: AuthFlowChoice::Default,
        };

        let result = SanitizedCreateCommand::try_from(cmd);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "--recovery-product-config is required if --recovery-board-config is specified."
        );
    }

    #[test]
    fn test_recovery_board_config_success() {
        let cmd = CreateCommand {
            product_config_board_config_combo: None,
            platform: None,
            product_config: Some("product".to_string()),
            recovery_product_config: Some("recovery_product".to_string()),
            board_config: Some("board".to_string()),
            recovery_board_config: Some("recovery_board".to_string()),
            output_name: None,
            output_version: None,
            output_version_file: None,
            tuf_keys: None,
            ota_manifest_key: None,
            stage: false,
            out: None,
            auth: AuthFlowChoice::Default,
        };

        let result = SanitizedCreateCommand::try_from(cmd);
        assert!(result.is_ok());
        let sanitized = result.unwrap();
        assert_eq!(sanitized.recovery_board_config, Some("recovery_board".to_string()));
    }
}
