// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::versioned_artifact_set::VersionedArtifactSet;
use anyhow::Result;
use assembly_artifact_cache::{ArtifactType, MOSIdentifier};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// A series of versions for a single artifact.
#[derive(Serialize, Deserialize, Debug)]
pub struct ArtifactVersionSeries {
    /// The name of the artifact.
    pub name: String,
    /// The type of the artifact.
    pub artifact_type: ArtifactType,
    /// The repository the artifact is stored in.
    pub repository: String,
    /// The available versions of the artifact.
    pub versions: Vec<MOSIdentifier>,
    /// An index pointing to the currently selected version of the artifact.
    pub current_artifact: usize,
    /// A range representing the remaining versions to be tested for the artifact.
    pub remaining_artifacts: Range<usize>,
}

impl ArtifactVersionSeries {
    /// Create a new ArtifactVersionSeries from a vector of MOS identifiers.
    pub(crate) fn from_versions(versions: Vec<MOSIdentifier>) -> Self {
        let first = versions
            .first()
            .expect("Artifact series should not be empty, check the data source.")
            .clone();
        let len = versions.len();
        Self {
            name: first.name,
            artifact_type: first.artifact_type,
            repository: first.repository,
            versions,
            current_artifact: if len == 0 { 0 } else { len / 2 },
            remaining_artifacts: 0..len,
        }
    }

    /// Returns the artifact at the given index.
    pub fn get_artifact_at_index(&self, index: usize) -> &MOSIdentifier {
        &self.versions[index]
    }
}

/// Manages the search space for the bisection.
#[derive(Serialize, Deserialize, Debug)]
pub struct SearchSpace {
    /// The platform artifact series.
    pub platform: ArtifactVersionSeries,
    /// The product artifact series.
    pub product: ArtifactVersionSeries,
    /// The board artifact series.
    pub board: ArtifactVersionSeries,
}

/// Represents the status of the bisection process.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum BisectionStatus {
    /// The bisection is ongoing.
    Continue,
    /// A culprit has been found. The two identifiers are the last known-good and first known-bad versions.
    CulpritFound(Box<MOSIdentifier>, Box<MOSIdentifier>),
    /// The search space has been exhausted without finding a culprit.
    Exhausted,
}

impl SearchSpace {
    /// Creates and initializes a new search space.
    pub fn new(
        platform_versions: Vec<MOSIdentifier>,
        product_versions: Vec<MOSIdentifier>,
        board_versions: Vec<MOSIdentifier>,
    ) -> Self {
        Self {
            platform: ArtifactVersionSeries::from_versions(platform_versions),
            product: ArtifactVersionSeries::from_versions(product_versions),
            board: ArtifactVersionSeries::from_versions(board_versions),
        }
    }

    /// Returns an iterator over all artifact series in the search space.
    pub fn iter_all_artifacts(&self) -> impl Iterator<Item = &ArtifactVersionSeries> {
        std::iter::once(&self.platform)
            .chain(std::iter::once(&self.product))
            .chain(std::iter::once(&self.board))
    }

    /// Get the set of artifacts at the current indices.
    pub fn get_current_versioned_artifact_set(&self) -> Result<VersionedArtifactSet> {
        Ok(VersionedArtifactSet {
            platform: self.platform.get_artifact_at_index(self.platform.current_artifact).clone(),
            product: self.product.get_artifact_at_index(self.product.current_artifact).clone(),
            board: self.board.get_artifact_at_index(self.board.current_artifact).clone(),
        })
    }

    /// Generates a formatted string representation of the search space for display.
    pub fn to_string_representation(&self, culprit: Option<&MOSIdentifier>) -> String {
        let mut output = String::new();
        output.push_str("Bisection Search Space:\n");

        let names: Vec<String> =
            self.iter_all_artifacts().map(|a| format!("{}/{}", a.artifact_type, a.name)).collect();
        let max_name_len = names.iter().map(|n| n.len()).max().unwrap_or(0);
        let max_artifacts_len =
            self.iter_all_artifacts().map(|v| v.versions.len()).max().unwrap_or(0);

        for artifacts in self.iter_all_artifacts() {
            if artifacts.versions.is_empty() {
                continue;
            }
            let name = format!("{}/{}", artifacts.artifact_type, artifacts.name);
            let padded_name = format!("{:<width$}", name, width = max_name_len);
            let range = &artifacts.remaining_artifacts;
            let current = artifacts.current_artifact;

            let mut visual = String::from("[");
            for j in 0..max_artifacts_len {
                if j < artifacts.versions.len() {
                    let is_culprit = culprit.map_or(false, |c| c == &artifacts.versions[j]);
                    if is_culprit {
                        visual.push_str(" * ");
                    } else if j == current && culprit.is_none() {
                        visual.push_str("\x1b[32m O \x1b[0m"); // Green
                    } else if range.contains(&j) {
                        visual.push_str(" o ");
                    } else {
                        visual.push_str("\x1b[90m X \x1b[0m"); // Dim/Gray
                    }
                } else {
                    visual.push_str(" - ");
                }
            }
            visual.push(']');

            output.push_str(&format!(
                "  {}: {} ({} remaining)\n",
                padded_name,
                visual,
                range.len()
            ));
        }
        output
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::SearchStrategy;

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
            platform: create_mock_artifact_series("platform", vec!["1", "2", "3", "4", "5"]),
            product: create_mock_artifact_series("product", vec!["a", "b", "c"]),
            board: create_mock_artifact_series("board", vec!["x", "y"]),
        }
    }

    #[test]
    fn test_update() {
        let mut search_space = create_mock_search_space();
        assert_eq!(search_space.platform.remaining_artifacts, 0..5);

        let strategy = crate::strategies::longest_dimension::LongestDimensionStrategy {};

        // First update: platform is longest (len 5). A "pass" shrinks it to the upper half.
        strategy.update(&mut search_space, true);
        assert_eq!(search_space.platform.remaining_artifacts, 2..5); // New range, len 3
        assert_eq!(search_space.platform.current_artifact, 3);

        // Second update: platform and product are tied (len 3). Tie-breaking selects platform.
        // A "fail" shrinks platform to the lower half of its current range.
        strategy.update(&mut search_space, false);
        assert_eq!(search_space.platform.remaining_artifacts, 2..3); // Lower half of 2..5 is 2..3
        assert_eq!(search_space.platform.current_artifact, 2);
        assert_eq!(search_space.product.remaining_artifacts, 0..3); // Product remains unchanged.
        assert_eq!(search_space.product.current_artifact, 1);

        // Third update: product is now longest (len 3 vs platform's len 1).
        // A "fail" shrinks product to its lower half.
        strategy.update(&mut search_space, false);
        assert_eq!(search_space.product.remaining_artifacts, 0..1);
        assert_eq!(search_space.product.current_artifact, 0);
        assert_eq!(search_space.platform.remaining_artifacts, 2..3); // Platform remains unchanged.
    }
}
