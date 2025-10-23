// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::{BisectionStatus, SearchSpace};
use crate::strategies::{Strategy, get_strategy};
use crate::versioned_artifact_set::VersionedArtifactSet;
use anyhow::{Result, ensure};
use assembly_artifact_cache::{MOSClient, MOSIdentifier};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use ffx_product_bundle_bisect_args::BisectCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use serde::{Deserialize, Serialize};

/// Represents the state of a bisection process.
#[derive(Serialize, Deserialize)]
pub struct BisectionPlan {
    /// The name of the product bundle being bisected.
    pub name: String,
    /// The last known-good version of the product bundle.
    pub from_success: String,
    /// The first known-bad version of the product bundle.
    pub to_failure: String,
    /// The home directory for storing bisection plans and artifacts.
    pub home: Utf8PathBuf,
    /// The search space for the bisection.
    pub search_space: SearchSpace,
    /// A list of the results of each bisection step.
    pub results: Vec<StepResult>,
    /// The current status of the bisection.
    pub status: BisectionStatus,
    /// The search strategy.
    pub strategy: Strategy,
}

impl BisectionPlan {
    /// Create a new Bisection Plan.
    pub async fn new<T: MOSClientTrait>(
        cmd: &BisectCommand,
        home: Utf8PathBuf,
        client: &mut T,
        writer: &mut SimpleWriter,
    ) -> Result<Self> {
        writer.line("Preparing bisection plan...")?;
        let strategy = get_strategy(cmd.strategy);

        // Retrieve release information for the initial PB (from_success).
        let starting_pb =
            Self::get_pb_release_info(client, &cmd.name, &cmd.from_success, writer).await?;

        // Retrieve release information for the final PB (to_failure).
        let ending_pb =
            Self::get_pb_release_info(client, &cmd.name, &cmd.to_failure, writer).await?;

        // Retrieve an interpolated version list between each starting and ending artifact.
        let search_space =
            Self::interpolate(client, starting_pb.clone(), ending_pb.clone(), writer).await?;

        let results = vec![
            StepResult { versioned_artifact_set: starting_pb, image_path: None, test_passed: true },
            StepResult { versioned_artifact_set: ending_pb, image_path: None, test_passed: false },
        ];
        let plan = Self {
            name: cmd.name.clone(),
            from_success: cmd.from_success.clone(),
            to_failure: cmd.to_failure.clone(),
            search_space,
            home,
            results,
            status: BisectionStatus::Continue,
            strategy,
        };
        Ok(plan)
    }

    /// Updates the plan with the result of a bisection step.
    pub fn update(&mut self, result: StepResult) -> BisectionStatus {
        self.results.push(result);
        let search_space = &mut self.search_space;
        self.strategy.as_dyn().update(search_space, self.results.last().unwrap().test_passed);
        self.status = self.strategy.as_dyn().should_continue(search_space, &self.results);
        self.status.clone()
    }

    /// Returns the current versioned artifact set to be tested.
    pub fn get_current_versioned_artifact_set(&self) -> Result<VersionedArtifactSet> {
        self.search_space.get_current_versioned_artifact_set()
    }

    /// Returns the path to the output directory for assembled images.
    pub fn out_dir(&self) -> Utf8PathBuf {
        self.home.join("out")
    }

    /// Returns the path to the directory for intermediate files.
    pub fn gen_dir(&self) -> Utf8PathBuf {
        self.home.join("gen")
    }

    /// Returns the path to the plan's save file.
    pub fn save_file(&self) -> Utf8PathBuf {
        self.home.join("plan.json")
    }

    /// Estimates the total number of steps for the bisection.
    pub fn estimate_total_steps(&self) -> usize {
        self.strategy.as_dyn().estimate_total_steps(&self.search_space)
    }

    /// A human-readable string describing the current status of the bisection.
    pub fn status(&self) -> String {
        // The number of completed steps is the number of results we have recorded.
        let completed_steps = self.results.len();

        // Create and return the formatted status string.
        format!(
            "{}: {}/~{} steps completed",
            self.name,
            completed_steps.saturating_sub(2), // minus starting and ending PB steps
            self.estimate_total_steps()
        )
    }

    async fn get_pb_release_info<T: MOSClientTrait>(
        client: &mut T,
        name: &String,
        version: &String,
        writer: &mut SimpleWriter,
    ) -> Result<VersionedArtifactSet> {
        writer.line(&format!(
            " - Retrieving release_info for {}@{}",
            name.clone(),
            version.clone()
        ))?;
        let response = client.get_pb_release_info(name.clone(), version.clone()).await?;
        let pb = VersionedArtifactSet::new_from_mos_ids(response)?;
        Ok(pb)
    }

    /// Contact MOS to retrieve the interpolated ReleaseInfo for every artifact
    /// in starting_pb and ending_pb.
    ///
    /// Every artifact in the starting product bundle must have a matching
    /// artifact in the ending product bundle or this will fail.
    async fn interpolate<T: MOSClientTrait>(
        client: &mut T,
        starting_pb: VersionedArtifactSet,
        ending_pb: VersionedArtifactSet,
        writer: &mut SimpleWriter,
    ) -> Result<SearchSpace> {
        // TODO: Confirm every artifact listed in START has a matching artifact in END
        // TODO: Handle special situations where artifacts are added or removed

        writer.line(" - Interpolating between versions")?;

        let platform =
            Self::interpolate_artifact(client, &starting_pb.platform, &ending_pb.platform, writer)
                .await?;
        let product =
            Self::interpolate_artifact(client, &starting_pb.product, &ending_pb.product, writer)
                .await?;
        let board =
            Self::interpolate_artifact(client, &starting_pb.board, &ending_pb.board, writer)
                .await?;

        let result = Ok(SearchSpace::new(platform, product, board));
        result
    }

    /// Helper function to interpolate a single artifact type, check the result,
    /// and print progress.
    async fn interpolate_artifact<T: MOSClientTrait>(
        client: &mut T,
        start: &MOSIdentifier,
        end: &MOSIdentifier,
        writer: &mut SimpleWriter,
    ) -> Result<Vec<MOSIdentifier>> {
        let versions = client.interpolate(start, end).await?;
        writer.line(&format!("  - {} [{} releases]", start.id_no_version(), versions.len()))?;
        ensure!(
            versions.first() == Some(start),
            "Interpolated {} artifacts for '{}' do not start with the expected artifact. Expected: {}, Got: {}",
            start.artifact_type,
            start.name,
            start.id(),
            versions.first().map(|p| p.id()).as_deref().unwrap_or("<None>")
        );
        ensure!(
            versions.last() == Some(end),
            "Interpolated {} artifacts for '{}' do not end with the expected artifact. Expected: {}, Got: {}",
            end.artifact_type,
            end.name,
            end.id(),
            versions.last().map(|p| p.id()).as_deref().unwrap_or("<None>")
        );
        Ok(versions)
    }
}

/// Represents the result of a single bisection step.
#[derive(Serialize, Deserialize)]
pub struct StepResult {
    /// The set of versioned artifacts used in this step.
    pub versioned_artifact_set: VersionedArtifactSet,
    /// The path to the image assembled in this step.
    pub image_path: Option<Utf8PathBuf>,
    /// Whether the test passed for this step.
    pub test_passed: bool,
}

/// A trait to abstract the MOS client, allowing for mock implementations in tests.
#[async_trait(?Send)]
pub trait MOSClientTrait {
    /// Retrieve release information for a product bundle.
    async fn get_pb_release_info(
        &mut self,
        name: String,
        version: String,
    ) -> Result<Vec<MOSIdentifier>>;
    /// Interpolate between two artifact versions to get a list of all intermediate versions.
    async fn interpolate(
        &self,
        start: &MOSIdentifier,
        end: &MOSIdentifier,
    ) -> Result<Vec<MOSIdentifier>>;
}

#[async_trait(?Send)]
impl MOSClientTrait for MOSClient {
    async fn get_pb_release_info(
        &mut self,
        name: String,
        version: String,
    ) -> Result<Vec<MOSIdentifier>> {
        MOSClient::get_pb_release_info(self, name, version).await
    }

    async fn interpolate(
        &self,
        start: &MOSIdentifier,
        end: &MOSIdentifier,
    ) -> Result<Vec<MOSIdentifier>> {
        MOSClient::interpolate(self, start, end).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_space::{ArtifactVersionSeries, BisectionStatus};
    use crate::strategies::Strategy;
    use crate::strategies::longest_dimension::LongestDimensionStrategy;
    use assembly_artifact_cache::{ArtifactType, MOSIdentifier};
    use tempfile::tempdir;

    fn create_mock_artifact_series(name: &str, versions: Vec<&str>) -> ArtifactVersionSeries {
        let mos_versions: Vec<MOSIdentifier> = versions
            .into_iter()
            .map(|v| MOSIdentifier {
                artifact_type: ArtifactType::Platform,
                name: name.to_string(),
                version: v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            })
            .collect();
        ArtifactVersionSeries::from_versions(mos_versions)
    }

    fn create_mock_search_space() -> SearchSpace {
        SearchSpace {
            platform: create_mock_artifact_series("platform", vec!["1", "2", "3"]),
            product: create_mock_artifact_series("product", vec!["a", "b", "c"]),
            board: create_mock_artifact_series("board", vec!["x", "y", "z"]),
        }
    }

    fn create_mock_versioned_artifact_set() -> VersionedArtifactSet {
        VersionedArtifactSet::new_from_mos_ids(vec![
            MOSIdentifier {
                artifact_type: ArtifactType::Platform,
                name: "platform".to_string(),
                version: "1".to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            MOSIdentifier {
                artifact_type: ArtifactType::Product,
                name: "product".to_string(),
                version: "a".to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            MOSIdentifier {
                artifact_type: ArtifactType::Board,
                name: "board".to_string(),
                version: "x".to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
        ])
        .unwrap()
    }

    #[test]
    fn test_update() {
        let temp_dir = tempdir().unwrap();
        let home = Utf8PathBuf::from_path_buf(temp_dir.path().to_path_buf()).unwrap();
        let mut plan = BisectionPlan {
            name: "test".to_string(),
            from_success: "1".to_string(),
            to_failure: "3".to_string(),
            home,
            strategy: Strategy::LongestDimension(LongestDimensionStrategy {}),
            search_space: create_mock_search_space(),
            results: vec![],
            status: BisectionStatus::Continue,
        };

        let result = StepResult {
            versioned_artifact_set: create_mock_versioned_artifact_set(),
            image_path: None,
            test_passed: true,
        };

        let status = plan.update(result);

        assert_eq!(plan.results.len(), 1);
        assert!(matches!(status, BisectionStatus::Continue));
        assert!(matches!(plan.status, BisectionStatus::Continue));
    }
}
