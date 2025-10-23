// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bisection_plan::{BisectionPlan, MOSClientTrait};
use crate::search_space::BisectionStatus;
use crate::versioned_artifact_set::VersionedArtifactSet;
use anyhow::{Context, Result};
use assembly_artifact_cache::{ArtifactCache, MOSIdentifier};
use assembly_cli_args::{ProductArgs, ValidationMode};
use assembly_config_schema::Architecture;
use camino::Utf8PathBuf;
use ffx_config::EnvironmentContext;
use ffx_product_bundle_bisect_args::BisectCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use std::fs::{self, File};
use std::io::{self, Write};
use {assembly_api, serde_json};

/// The BisectionController runs the entire bisection process.
/// It maintains a reference to the BisectionPlan and manages the assembly artifact cache.
pub struct BisectionController<'a> {
    /// The bisection plan, which maintains the state of the bisection.
    pub plan: BisectionPlan,
    /// The artifact cache, for downloading and storing artifacts.
    cache: ArtifactCache,
    /// The writer for printing output to the user.
    writer: &'a mut SimpleWriter,
    /// The ffx environment context.
    env_context: &'a EnvironmentContext,
}

impl<'a> BisectionController<'a> {
    /// Creates a new BisectionController.
    /// If a plan file already exists at `plan_home`, this will prompt the user
    /// to either continue the existing plan or delete it and start a new one.
    pub async fn new(
        cmd: BisectCommand,
        plan_home: Utf8PathBuf,
        mut client: impl MOSClientTrait,
        cache: ArtifactCache,
        writer: &'a mut SimpleWriter,
        env_context: &'a EnvironmentContext,
    ) -> Result<Self> {
        let plan_file = plan_home.join("plan.json");

        // Load existing plan if it exists.
        if plan_file.is_file() {
            let file = File::open(&plan_file)
                .with_context(|| format!("Failed to open bisection plan from {}", plan_file))?;
            let reader = std::io::BufReader::new(file);
            let plan: BisectionPlan = serde_json::from_reader(reader).with_context(|| {
                format!("Failed to deserialize bisection plan from {}", plan_file)
            })?;

            writer.line(format!("Found previous bisection:\n -> [{}]\n", &plan.status()))?;
            if confirm_action(writer).context("Failed to get user confirmation")? {
                writer.line(format!("\nContinuing existing plan: {}\n", plan_file))?;
                return Ok(Self { plan, cache, writer, env_context });
            } else {
                writer.line(format!("\nDeleting existing plan: {}...", plan_file))?;
                fs::remove_file(&plan_file).with_context(|| {
                    format!("Failed to delete existing plan file at {}", plan_file)
                })?;
                writer.line(format!("Plan {} deleted.\n", plan_file))?;
            }
        }

        // No plan exists, or the user chose to delete the old one. Create a new one.
        let plan = BisectionPlan::new(&cmd, plan_home, &mut client, writer).await?;

        Ok(Self { plan, cache, writer, env_context })
    }

    /// Run the bisection plan.
    pub async fn run(&mut self) -> Result<()> {
        // If the plan is already complete, print the result and exit.
        if !matches!(self.plan.status, BisectionStatus::Continue) {
            self.print("Bisection plan already completed.")?;
            if let BisectionStatus::CulpritFound(good, bad) = self.plan.status.clone() {
                self.print_culprit(&good, &bad);
            } else if let BisectionStatus::Exhausted = self.plan.status {
                self.print(" -> Bisection search space exhausted without finding a culprit.")?;
            }
            return Ok(());
        }

        // Execute the search strategy.
        loop {
            match self.step().await? {
                BisectionStatus::Continue => continue,
                BisectionStatus::CulpritFound(good, bad) => {
                    self.print_culprit(&good, &bad);
                    break;
                }
                BisectionStatus::Exhausted => {
                    self.print("Bisection search space exhausted without finding a culprit.")?;
                    break;
                }
            }
        }
        Ok(())
    }

    /// step() runs one step of the bisection strategy.
    ///
    /// * Downloads the current artifacts and runs assembly to build a product bundle
    /// * Prompts the user to run a test with the generated image
    /// * Reports the test results to the strategy.
    async fn step(&mut self) -> Result<BisectionStatus> {
        self.print("====================")?;
        self.print(&self.plan.status())?;
        self.print(&self.plan.search_space.to_string_representation(None))?;

        // Get current versioned artifact set from the plan.
        let versioned_artifact_set = self.plan.get_current_versioned_artifact_set()?;

        // Download the current  artifacts and assemble them into a PB.
        let product_bundle_path = self.assemble(versioned_artifact_set.clone()).await?;

        // Instruct the user to flash the pb, and wait for tests results.
        // TODO: support automated tests
        let test_passed = self.prompt_for_manual_test(product_bundle_path.as_str())?;

        // Report results
        let result = crate::bisection_plan::StepResult {
            versioned_artifact_set,
            image_path: Some(product_bundle_path),
            test_passed,
        };
        let status = self.plan.update(result);
        self.save()?;
        Ok(status)
    }

    /// Ask the user to run a test with the given fuchsia image,
    /// and return the results.
    fn prompt_for_manual_test(&mut self, product_bundle_path: &str) -> Result<bool> {
        self.print("")?;
        self.print(&format!("The Fuchsia Image is located here: {}", product_bundle_path))?;
        self.print("Flash it to a local device by running:\n")?;

        self.print(&format!("  ffx target flash {}\n", product_bundle_path))?;
        self.print("Then run a test to determine whether or not the original issue remains.")?;
        self.print("-----")?;

        loop {
            self.writer.write_all(b"\nDoes the test pass with this image? (y/n) ")?;
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            match input.trim().to_lowercase().as_str() {
                "y" | "yes" | "pass" => return Ok(true),
                "n" | "no" | "fail" => return Ok(false),
                _ => {
                    self.print(
                        "Invalid input. Please enter 'y', 'yes', 'pass', or 'n', 'no', 'fail'.",
                    )?;
                }
            }
        }
    }

    fn print(&mut self, message: &str) -> Result<(), ffx_writer::Error> {
        self.writer.line(message)
    }

    /// Print information about the culprit once it has been found.
    fn print_culprit(&mut self, good: &MOSIdentifier, bad: &MOSIdentifier) {
        let _ = self.print(&self.plan.search_space.to_string_representation(Some(bad)));
        let _ = self.print(&format!(
            "\n************\nThe test passed with a product bundle containing {} artifact \"{}\" at version \"{}\", but it failed at version \"{}\".",
            good.artifact_type,
            good.name,
            good.version,
            bad.version
        ));
        let _ = self.print(&format!("\nThe bug was introduced in {:?}.\n", bad));
    }

    /// Run assembly with the current collection of assembly artifacts
    /// to generate a flashable fuchsia image.
    async fn assemble(&mut self, pb: VersionedArtifactSet) -> Result<Utf8PathBuf> {
        // Ensure all of the artifacts have been downloaded.
        let product_config_path = self.cache.resolve_product(pb.product.id()).await?;
        let board_config_path = self.cache.resolve_board(pb.board.id()).await?;
        let platform_config_path =
            self.cache.resolve_platform(Some(pb.platform.id()), &Architecture::X64).await?;

        // Perform assembly
        let product_args = ProductArgs {
            product: product_config_path,
            board_config: board_config_path,
            outdir: self.plan.out_dir(),
            gendir: self.plan.gen_dir(),
            input_bundles_dir: platform_config_path,
            package_validation: Some(ValidationMode::Off),
            custom_kernel_aib: None,
            custom_boot_shim_aib: None,
            suppress_overrides_warning: false,
            developer_overrides: None,
            include_example_aib_for_tests: Some(false),
            mode: Default::default(),
        };

        self.writer.write_all(b"Assembling image... ")?;
        io::stdout().flush()?;
        let create_system_outputs = assembly_api::assemble(&self.env_context, product_args)?;
        self.print("Done.")?;

        Ok(create_system_outputs.outdir)
    }

    /// Saves the current state of the BisectionPlan to its `save_file` path.
    /// The struct is serialized as a JSON string.
    pub fn save(&mut self) -> std::io::Result<()> {
        let _ = self.print(&format!(
            "\nSaving current status: [{}] {} ...",
            self.plan.status(),
            self.plan.save_file()
        ));

        let json_string =
            serde_json::to_string_pretty(&self.plan).expect("Failed to serialize BisectionPlan");

        // Create or truncate the save file.
        let mut file = File::create(&self.plan.save_file())?;

        // Write the JSON string to the file.
        file.write_all(json_string.as_bytes())
    }
}

// Function to handle user confirmation
fn confirm_action(writer: &mut SimpleWriter) -> anyhow::Result<bool> {
    loop {
        writer.write_all(b"Continue? (y/n) ")?;
        writer.flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.to_lowercase().trim() {
            "yes" | "y" => return Ok(true),
            "no" | "n" => return Ok(false),
            "" => {
                writer.line("Invalid input. Please enter 'y', 'yes', 'n', or 'no'.")?;
            }
            _ => {
                writer.line("Invalid input. Please enter 'y', 'yes', 'n', or 'no'.")?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisection_plan::MOSClientTrait;
    use crate::search_space::{ArtifactVersionSeries, SearchSpace};
    use assembly_artifact_cache::ArtifactType;
    use async_trait::async_trait;
    use ffx_config::EnvironmentContext;
    use ffx_product_bundle_bisect_args as args;
    use futures_lite::future::block_on;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use tempfile::tempdir;
    struct MockMOSClient {
        responses: RefCell<VecDeque<Result<Vec<MOSIdentifier>>>>,
    }

    impl MockMOSClient {
        fn new(responses: Vec<Result<Vec<MOSIdentifier>>>) -> Self {
            Self { responses: RefCell::new(responses.into()) }
        }
    }

    #[async_trait(?Send)]
    impl MOSClientTrait for MockMOSClient {
        async fn get_pb_release_info(
            &mut self,
            _name: String,
            _version: String,
        ) -> Result<Vec<MOSIdentifier>> {
            self.responses.borrow_mut().pop_front().unwrap()
        }

        async fn interpolate(
            &self,
            _start: &MOSIdentifier,
            _end: &MOSIdentifier,
        ) -> Result<Vec<MOSIdentifier>> {
            self.responses.borrow_mut().pop_front().unwrap()
        }
    }

    fn setup_cmd() -> BisectCommand {
        BisectCommand {
            name: "test.name".to_string(),
            from_success: "good".to_string(),
            to_failure: "bad".to_string(),
            out_dir: None,
            gen_dir: None,
            auth: pbms::AuthFlowChoice::Default,
            strategy: args::Strategy::LongestDimension,
        }
    }

    fn mock_vas_as_vec(p_v: &str, r_v: &str, b_v: &str) -> Vec<MOSIdentifier> {
        vec![
            MOSIdentifier {
                artifact_type: ArtifactType::Platform,
                name: "platform".to_string(),
                version: p_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            MOSIdentifier {
                artifact_type: ArtifactType::Product,
                name: "product".to_string(),
                version: r_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            MOSIdentifier {
                artifact_type: ArtifactType::Board,
                name: "board".to_string(),
                version: b_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
        ]
    }

    async fn setup_controller<'a>(
        responses: Vec<Result<Vec<MOSIdentifier>>>,
        writer: &'a mut SimpleWriter,
        env_context: &'a EnvironmentContext,
    ) -> (BisectionController<'a>, tempfile::TempDir) {
        let gcs_client = gcs::client::Client::initial().unwrap();
        let dir = tempdir().unwrap();
        let home = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let client = MockMOSClient::new(responses);

        let cache = ArtifactCache::new(None, gcs_client).unwrap();

        let controller =
            BisectionController::new(setup_cmd(), home, client, cache, writer, env_context)
                .await
                .unwrap();
        (controller, dir)
    }

    // Tests that a new BisectionController can be created successfully.
    #[test]
    fn test_new() {
        let start_pb = mock_vas_as_vec("1.0", "1.0", "1.0");
        let end_pb = mock_vas_as_vec("2.0", "2.0", "2.0");
        let responses = vec![
            Ok(start_pb.clone()),
            Ok(end_pb.clone()),
            Ok(vec![start_pb[0].clone(), end_pb[0].clone()]),
            Ok(vec![start_pb[1].clone(), end_pb[1].clone()]),
            Ok(vec![start_pb[2].clone(), end_pb[2].clone()]),
        ];
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        let env_context = EnvironmentContext::default();
        let (controller, _dir) = block_on(setup_controller(responses, &mut writer, &env_context));
        assert_eq!(controller.plan.name, "test.name");
    }

    // Tests that the bisection plan can be saved to a file.
    #[test]
    fn test_save() {
        let start_pb = mock_vas_as_vec("1.0", "1.0", "1.0");
        let end_pb = mock_vas_as_vec("2.0", "2.0", "2.0");
        let responses = vec![
            Ok(start_pb.clone()),
            Ok(end_pb.clone()),
            Ok(vec![start_pb[0].clone(), end_pb[0].clone()]),
            Ok(vec![start_pb[1].clone(), end_pb[1].clone()]),
            Ok(vec![start_pb[2].clone(), end_pb[2].clone()]),
        ];
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        let env_context = EnvironmentContext::default();
        let (mut controller, _dir) =
            block_on(setup_controller(responses, &mut writer, &env_context));
        let save_file = controller.plan.save_file();
        assert!(!save_file.exists());
        controller.save().unwrap();
        assert!(save_file.exists());
        let content = std::fs::read_to_string(save_file).unwrap();
        let loaded_plan: BisectionPlan = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded_plan.name, "test.name");
    }

    // Tests that the culprit is printed correctly.
    #[test]
    fn test_print_culprit() {
        let start_pb = mock_vas_as_vec("1.0", "1.0", "1.0");
        let end_pb = mock_vas_as_vec("2.0", "2.0", "2.0");
        let responses = vec![
            Ok(start_pb.clone()),
            Ok(end_pb.clone()),
            Ok(vec![start_pb[0].clone(), end_pb[0].clone()]),
            Ok(vec![start_pb[1].clone(), end_pb[1].clone()]),
            Ok(vec![start_pb[2].clone(), end_pb[2].clone()]),
        ];
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        let env_context = EnvironmentContext::default();
        let (mut controller, _dir) =
            block_on(setup_controller(responses, &mut writer, &env_context));
        let good = MOSIdentifier {
            artifact_type: ArtifactType::Platform,
            name: "fuchsia".to_string(),
            version: "1.2.3".to_string(),
            repository: "fuchsia".to_string(),
            cipd: None,
        };
        let bad = MOSIdentifier {
            artifact_type: ArtifactType::Platform,
            name: "fuchsia".to_string(),
            version: "1.2.4".to_string(),
            repository: "fuchsia".to_string(),
            cipd: None,
        };

        let mock_series = ArtifactVersionSeries::from_versions(vec![good.clone()]);
        controller.plan.search_space =
            SearchSpace::new(vec![good.clone()], vec![good.clone()], vec![good.clone()]);
        controller.plan.search_space.platform = mock_series;

        controller.print_culprit(&good, &bad);
        let output = test_buffers.stdout.clone().into_string();
        let expected_substring = "The test passed with a product bundle containing platform artifact \"fuchsia\" at version \"1.2.3\", but it failed at version \"1.2.4\".";
        assert!(
            output.contains(expected_substring),
            "Expected output to contain:\n---\n{}\n---\nActual output was:\n---\n{}\n---",
            expected_substring,
            output
        );
    }

    // Tests that the run function exits early if the plan is already complete.
    #[test]
    fn test_run_already_complete() {
        let start_pb = mock_vas_as_vec("1.0", "1.0", "1.0");
        let end_pb = mock_vas_as_vec("2.0", "2.0", "2.0");

        // Test case for BisectionStatus::Exhausted
        let responses = vec![
            Ok(start_pb.clone()),
            Ok(end_pb.clone()),
            Ok(vec![start_pb[0].clone(), end_pb[0].clone()]),
            Ok(vec![start_pb[1].clone(), end_pb[1].clone()]),
            Ok(vec![start_pb[2].clone(), end_pb[2].clone()]),
        ];
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        let env_context = EnvironmentContext::default();
        let (mut controller, _dir) =
            block_on(setup_controller(responses, &mut writer, &env_context));
        controller.plan.status = BisectionStatus::Exhausted;
        let result = block_on(controller.run());
        assert!(result.is_ok());
        let output = test_buffers.stdout.clone().into_string();
        assert!(output.contains("Bisection plan already completed."));
        assert!(output.contains("Bisection search space exhausted without finding a culprit."));

        // Test case for BisectionStatus::CulpritFound
        let responses = vec![
            Ok(start_pb.clone()),
            Ok(end_pb.clone()),
            Ok(vec![start_pb[0].clone(), end_pb[0].clone()]),
            Ok(vec![start_pb[1].clone(), end_pb[1].clone()]),
            Ok(vec![start_pb[2].clone(), end_pb[2].clone()]),
        ];
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        let env_context = EnvironmentContext::default();
        let (mut controller, _dir) =
            block_on(setup_controller(responses, &mut writer, &env_context));
        let good = MOSIdentifier {
            artifact_type: ArtifactType::Platform,
            name: "fuchsia".to_string(),
            version: "1.2.3".to_string(),
            repository: "fuchsia".to_string(),
            cipd: None,
        };
        let bad = MOSIdentifier {
            artifact_type: ArtifactType::Platform,
            name: "fuchsia".to_string(),
            version: "1.2.4".to_string(),
            repository: "fuchsia".to_string(),
            cipd: None,
        };
        controller.plan.status =
            BisectionStatus::CulpritFound(Box::new(good.clone()), Box::new(bad));
        let mock_series = ArtifactVersionSeries::from_versions(vec![good.clone()]);
        controller.plan.search_space =
            SearchSpace::new(vec![good.clone()], vec![good.clone()], vec![good.clone()]);
        controller.plan.search_space.platform = mock_series;
        let result = block_on(controller.run());
        assert!(result.is_ok());
        let output = test_buffers.stdout.clone().into_string();
        assert!(output.contains("Bisection plan already completed."));
    }
}
