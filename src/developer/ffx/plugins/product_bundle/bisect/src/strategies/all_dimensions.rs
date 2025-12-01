// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::{BisectionStatus, SearchSpace};
use crate::strategies::{SearchStrategy, util};
use serde::{Deserialize, Serialize};

/// A hybrid bisection strategy that operates in two phases.
///
/// Phase 1: Broad Search. For speed, this phase bisects all dimensions
/// simultaneously to narrow the search space quickly.
///
/// Phase 2: Precise Search. To ensure a culprit can be identified, this phase
/// switches to bisecting only the single longest dimension. This guarantees that
/// test runs will differ by only one variable at a time, which is required by
/// the `find_culprit` logic.
///
/// The switch from Phase 1 to Phase 2 occurs when the longest remaining
/// dimension has a length less than or equal to `space.num_dimensions() + 1`.
#[derive(Serialize, Deserialize)]
pub struct AllDimensionsStrategy;

impl SearchStrategy for AllDimensionsStrategy {
    fn update(&self, space: &mut SearchSpace, test_passed: bool) {
        // Determine how many artifacts are left to search across all dimensions.
        let remaining_artifacts_len =
            space.iter_all_artifacts().map(|s| s.remaining_artifacts.len()).max().unwrap_or(0);

        // Determine which phase to execute.
        // If the search space is large, bisect all dimensions for speed (Phase 1).
        // If the search space is small, switch to only bisecting the longest dimension
        // to ensure we can isolate the culprit (Phase 2).
        if remaining_artifacts_len > space.num_dimensions() + 1 {
            // Phase 1: Bisect all dimensions.
            for series in [&mut space.platform, &mut space.product, &mut space.board] {
                if series.remaining_artifacts.len() <= 1 {
                    continue;
                }
                let view = series.remaining_artifacts.clone();
                let midpoint = view.start + view.len() / 2;
                if test_passed {
                    // We know [midpoint] is good, so we can remove it from the list.
                    series.remaining_artifacts = (midpoint + 1)..view.end;
                } else {
                    // This version might be the culprit, so keep midpoint in the list.
                    series.remaining_artifacts = view.start..(midpoint + 1);
                }
            }
        } else {
            // Phase 2: Bisect only the longest dimension.
            util::halve_longest_dimension(space, test_passed);
        }

        // After shrinking the search space, always update the midpoint pointers for the next step.
        let range = &space.platform.remaining_artifacts;
        space.platform.current_artifact = range.start + (range.len() / 2);
        let range = &space.product.remaining_artifacts;
        space.product.current_artifact = range.start + (range.len() / 2);
        let range = &space.board.remaining_artifacts;
        space.board.current_artifact = range.start + (range.len() / 2);
    }

    fn should_continue(
        &self,
        space: &SearchSpace,
        results: &Vec<crate::bisection_plan::StepResult>,
    ) -> BisectionStatus {
        util::should_continue(space, results)
    }

    /// Estimates the total number of steps required for the bisection.
    ///
    /// This strategy works in two phases:
    /// 1. A broad search phase where all dimensions are bisected simultaneously.
    ///    The number of steps in this phase is estimated by `ceil(log2(longest_dimension_length))`.
    /// 2. A precise search phase where only one dimension is bisected at a time to isolate
    ///    the culprit. The worst-case for this phase is `num_dimensions()` steps.
    ///
    /// The total estimate is the sum of steps from both phases.
    fn estimate_total_steps(&self, space: &SearchSpace) -> usize {
        let longest_dim_len =
            space.iter_all_artifacts().map(|artifacts| artifacts.versions.len()).max().unwrap_or(0);

        let phase1_steps =
            if longest_dim_len <= 1 { 0.0 } else { (longest_dim_len as f64).log2().ceil() };

        phase1_steps as usize + space.num_dimensions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_space::ArtifactVersionSeries;
    use assembly_artifact_cache::{ArtifactType, MOSIdentifier};

    // ##################################################################
    // # Test Helpers
    // ##################################################################

    fn create_mock_artifact_series(
        artifact_type: ArtifactType,
        name: &str,
        versions: Vec<&str>,
    ) -> ArtifactVersionSeries {
        let mos_versions: Vec<MOSIdentifier> = versions
            .into_iter()
            .map(|v| MOSIdentifier {
                artifact_type: artifact_type.clone(),
                name: name.to_string(),
                version: v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            })
            .collect();
        if mos_versions.is_empty() {
            return ArtifactVersionSeries {
                name: name.to_string(),
                artifact_type,
                repository: "fuchsia".to_string(),
                versions: vec![],
                current_artifact: 0,
                remaining_artifacts: 0..0,
            };
        }
        ArtifactVersionSeries::from_versions(mos_versions)
    }

    fn create_mock_search_space(
        platform_versions: Vec<&str>,
        product_versions: Vec<&str>,
        board_versions: Vec<&str>,
    ) -> SearchSpace {
        SearchSpace {
            platform: create_mock_artifact_series(
                ArtifactType::Platform,
                "platform",
                platform_versions,
            ),
            product: create_mock_artifact_series(
                ArtifactType::Product,
                "product",
                product_versions,
            ),
            board: create_mock_artifact_series(ArtifactType::Board, "board", board_versions),
        }
    }

    // ##################################################################
    // # 1. Core Bisection Logic (`update` method)
    // ##################################################################

    #[test]
    /// Phase 1: Verifies a passing test shrinks all dimensions to their upper half.
    fn test_phase1_update_with_pass_shrinks_all_dimensions() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"], // len 8
            vec!["a", "b", "c", "d", "e"],                // len 5
            vec!["w", "x", "y", "z"],                     // len 4
        );
        let strategy = AllDimensionsStrategy;

        // Longest dim is 8, which is > num_dimensions(3)+1. Should be Phase 1.
        assert!(space.platform.remaining_artifacts.len() > space.num_dimensions() + 1);

        // Action: Report a passing test.
        strategy.update(&mut space, true);

        // Assert: All dimensions are shrunk to their upper half.
        // Platform: midpoint=4, new range=(4+1)..8 = 5..8
        assert_eq!(space.platform.remaining_artifacts, 5..8);
        // Product: midpoint=2, new range=(2+1)..5 = 3..5
        assert_eq!(space.product.remaining_artifacts, 3..5);
        // Board: midpoint=2, new range=(2+1)..4 = 3..4
        assert_eq!(space.board.remaining_artifacts, 3..4);
    }

    #[test]
    /// Phase 1: Verifies a failing test shrinks all dimensions to their lower half.
    fn test_phase1_update_with_fail_shrinks_all_dimensions() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"], // len 8
            vec!["a", "b", "c", "d", "e"],                // len 5
            vec!["w", "x", "y", "z"],                     // len 4
        );
        let strategy = AllDimensionsStrategy;

        // Longest dim is 8, which is > num_dimensions(3)+1. Should be Phase 1.
        assert!(space.platform.remaining_artifacts.len() > space.num_dimensions() + 1);

        // Action: Report a failing test.
        strategy.update(&mut space, false);

        // Assert: All dimensions are shrunk to their lower half.
        // Platform: midpoint=4, new range=0..(4+1) = 0..5
        assert_eq!(space.platform.remaining_artifacts, 0..5);
        // Product: midpoint=2, new range=0..(2+1) = 0..3
        assert_eq!(space.product.remaining_artifacts, 0..3);
        // Board: midpoint=2, new range=0..(2+1) = 0..3
        assert_eq!(space.board.remaining_artifacts, 0..3);
    }

    #[test]
    /// Phase 2: Verifies strategy switches to shrinking only the longest dimension.
    fn test_phase2_update_shrinks_only_longest_dimension() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4"], // len 4
            vec!["a", "b"],           // len 2
            vec!["x", "y"],           // len 2
        );
        let strategy = AllDimensionsStrategy;

        // Longest dim is 4, which is <= num_dimensions(3)+1. Should be Phase 2.
        assert!(space.platform.remaining_artifacts.len() <= space.num_dimensions() + 1);

        // Action: Report a failing test.
        strategy.update(&mut space, false);

        // Assert: Only the longest dimension (platform) is shrunk.
        // Platform: midpoint=2, new range=0..(2) = 0..2
        assert_eq!(space.platform.remaining_artifacts, 0..2);
        // Others are unchanged.
        assert_eq!(space.product.remaining_artifacts, 0..2);
        assert_eq!(space.board.remaining_artifacts, 0..2);
    }

    #[test]
    /// Verifies that dimensions of length <= 1 are not modified.
    fn test_update_on_short_dimensions_is_noop() {
        // Note: dimensions of length 0 are invalid.
        let mut space =
            create_mock_search_space(vec!["1", "2", "3", "4", "5", "6"], vec!["a"], vec!["x"]);
        let strategy = AllDimensionsStrategy;
        strategy.update(&mut space, false);

        // Long dimension is shrunk
        assert_eq!(space.platform.remaining_artifacts, 0..4);
        // Short dimensions are not.
        assert_eq!(space.product.remaining_artifacts, 0..1);
    }

    // ##################################################################
    // # 2. Estimation Logic (`estimate_total_steps` method)
    // ##################################################################

    #[test]
    /// Verifies that the step estimation logic is correct.
    fn test_estimate_total_steps_calculates_correctly() {
        // Longest is 8. ceil(log2(8)) = 3. num_dimensions = 3. Total = 3 + 3 = 6.
        let space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"],
            vec!["a", "b", "c", "d", "e"],
            vec!["x"],
        );
        let strategy = AllDimensionsStrategy;
        assert_eq!(strategy.estimate_total_steps(&space), 6);

        // Longest is 1. ceil(log2(1)) = 0. num_dimensions = 3. Total = 0 + 3 = 3.
        let space_single = create_mock_search_space(vec!["1"], vec!["a"], vec!["x"]);
        assert_eq!(strategy.estimate_total_steps(&space_single), 3);

        // Longest is 2. ceil(log2(2)) = 1. num_dimensions = 3. Total = 1 + 3 = 4.
        let space_two = create_mock_search_space(vec!["1", "2"], vec!["a", "b"], vec!["x", "y"]);
        assert_eq!(strategy.estimate_total_steps(&space_two), 4);
    }
}
