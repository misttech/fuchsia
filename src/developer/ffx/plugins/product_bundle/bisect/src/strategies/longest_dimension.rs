// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::{BisectionStatus, SearchSpace};
use crate::strategies::{SearchStrategy, util};
use serde::{Deserialize, Serialize};

/// A bisection strategy that always chooses the midpoint of the longest remaining artifact series.
#[derive(Serialize, Deserialize)]
pub struct LongestDimensionStrategy;

impl SearchStrategy for LongestDimensionStrategy {
    fn update(&self, space: &mut SearchSpace, test_passed: bool) {
        util::halve_longest_dimension(space, test_passed);

        // Now that we've reduced the size of the remaining search window, go through and update
        // the midpoint pointers to reflect that change.
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
    /// This strategy estimates the total steps as the sum of `ceil(log2(len))`
    /// for each dimension's version list. This is because, in the worst case,
    /// each dimension might need to be bisected individually.
    fn estimate_total_steps(&self, space: &SearchSpace) -> usize {
        space
            .iter_all_artifacts()
            .map(|artifacts| {
                let len = artifacts.versions.len();
                if len <= 1 { 0.0 } else { (len as f64).log2().ceil() }
            })
            .map(|steps| steps as usize)
            .sum()
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
    /// Verifies that a passing test correctly shrinks the longest dimension's
    /// search space to the upper half.
    fn test_update_with_pass_shrinks_longest_dimension_correctly() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"],
            vec!["a", "b"],
            vec!["x", "y"],
        );
        let strategy = LongestDimensionStrategy;

        // Initial state: platform is longest (len 8), range is 0..8, current is 4.
        assert_eq!(space.platform.remaining_artifacts, 0..8);
        assert_eq!(space.platform.current_artifact, 4);

        // Action: Report a passing test.
        strategy.update(&mut space, true);

        // Assert: Platform's range is now the upper half, and current is updated.
        // The new range is (midpoint + 1)..end = 5..8. The new current is 5 + (3 / 2) = 6.
        assert_eq!(space.platform.remaining_artifacts, 5..8);
        assert_eq!(space.platform.current_artifact, 6);
        // Other dimensions are unchanged.
        assert_eq!(space.product.remaining_artifacts, 0..2);
        assert_eq!(space.board.remaining_artifacts, 0..2);
    }

    #[test]
    /// Verifies that a failing test correctly shrinks the longest dimension's
    /// search space to the lower half.
    fn test_update_with_fail_shrinks_longest_dimension_correctly() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"],
            vec!["a", "b"],
            vec!["x", "y"],
        );
        let strategy = LongestDimensionStrategy;

        // Initial state: platform is longest (len 8), range is 0..8, current is 4.
        assert_eq!(space.platform.remaining_artifacts, 0..8);
        assert_eq!(space.platform.current_artifact, 4);

        // Action: Report a failing test.
        strategy.update(&mut space, false);

        // Assert: Platform's range is now the lower half, and current is updated.
        // The new range is start..midpoint = 0..4. The new current is 0 + (4/2) = 2.
        assert_eq!(space.platform.remaining_artifacts, 0..4);
        assert_eq!(space.platform.current_artifact, 2);
        // Other dimensions are unchanged.
        assert_eq!(space.product.remaining_artifacts, 0..2);
        assert_eq!(space.board.remaining_artifacts, 0..2);
    }

    #[test]
    /// Verifies the tie-breaking logic: if multiple dimensions have the same
    /// longest length, `platform` is chosen first.
    fn test_tie_breaking_prefers_platform() {
        let mut space = create_mock_search_space(
            vec!["1", "2", "3", "4"],
            vec!["a", "b", "c", "d"],
            vec!["x", "y"],
        );
        let strategy = LongestDimensionStrategy;

        // Action: Report a failing test.
        strategy.update(&mut space, false);

        // Assert: Platform is shrunk, product is not.
        assert_eq!(space.platform.remaining_artifacts, 0..2);
        assert_eq!(space.product.remaining_artifacts, 0..4);
        assert_eq!(space.board.remaining_artifacts, 0..2);
    }

    #[test]
    /// Verifies the tie-breaking logic: if product and board are tied for longest,
    /// `product` is chosen.
    fn test_tie_breaking_prefers_product_over_board() {
        let mut space = create_mock_search_space(
            vec!["1", "2"],
            vec!["a", "b", "c", "d"],
            vec!["w", "x", "y", "z"],
        );
        let strategy = LongestDimensionStrategy;

        // Action: Report a failing test.
        strategy.update(&mut space, false);

        // Assert: Product is shrunk, board is not.
        assert_eq!(space.platform.remaining_artifacts, 0..2);
        assert_eq!(space.product.remaining_artifacts, 0..2);
        assert_eq!(space.board.remaining_artifacts, 0..4);
    }

    // ##################################################################
    // # 2. Edge Cases (`update` method)
    // ##################################################################

    #[test]
    /// Verifies that midpoint calculations work correctly for both even and
    /// odd length dimensions.
    fn test_update_with_even_and_odd_length_dimensions() {
        let strategy = LongestDimensionStrategy;

        // Odd length (7)
        let mut space_odd =
            create_mock_search_space(vec!["1", "2", "3", "4", "5", "6", "7"], vec![], vec![]);
        assert_eq!(space_odd.platform.remaining_artifacts, 0..7);
        strategy.update(&mut space_odd, false); // Fail -> lower half
        assert_eq!(space_odd.platform.remaining_artifacts, 0..3); // Midpoint is 3. Range is 0..3.

        // Even length (8)
        let mut space_even =
            create_mock_search_space(vec!["1", "2", "3", "4", "5", "6", "7", "8"], vec![], vec![]);
        assert_eq!(space_even.platform.remaining_artifacts, 0..8);
        strategy.update(&mut space_even, true); // Pass -> upper half
        assert_eq!(space_even.platform.remaining_artifacts, 5..8); // Midpoint is 4. Range is 5..8.
    }

    #[test]
    /// Verifies that a dimension of length 2 is correctly shrunk to length 1.
    fn test_update_on_dimension_of_length_two() {
        let mut space = create_mock_search_space(vec!["1", "2"], vec![], vec![]);
        let strategy = LongestDimensionStrategy;

        strategy.update(&mut space, false);
        assert_eq!(space.platform.remaining_artifacts, 0..1);
    }

    #[test]
    /// Verifies that calling update on a dimension of length 1 or 0 does nothing.
    fn test_update_on_dimension_of_length_one_does_nothing() {
        let mut space = create_mock_search_space(vec!["1"], vec![], vec![]);
        let strategy = LongestDimensionStrategy;

        let initial_range = space.platform.remaining_artifacts.clone();
        strategy.update(&mut space, false);
        assert_eq!(space.platform.remaining_artifacts, initial_range);

        let mut space_empty = create_mock_search_space(vec![], vec![], vec![]);
        let initial_range_empty = space_empty.platform.remaining_artifacts.clone();
        strategy.update(&mut space_empty, false);
        assert_eq!(space_empty.platform.remaining_artifacts, initial_range_empty);
    }
}
