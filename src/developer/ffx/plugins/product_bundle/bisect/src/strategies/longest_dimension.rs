// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::{BisectionStatus, SearchSpace};
use crate::strategies::SearchStrategy;
use serde::{Deserialize, Serialize};

/// A bisection strategy that always chooses the midpoint of the longest remaining artifact series.
#[derive(Serialize, Deserialize)]
pub struct LongestDimensionStrategy;

impl SearchStrategy for LongestDimensionStrategy {
    fn update(&self, space: &mut SearchSpace, test_passed: bool) {
        let platform_len = space.platform.remaining_artifacts.len();
        let product_len = space.product.remaining_artifacts.len();
        let board_len = space.board.remaining_artifacts.len();

        // Determine which artifact has the most versions remaining to search.
        let longest_series = if platform_len >= product_len && platform_len >= board_len {
            &mut space.platform
        } else if product_len >= platform_len && product_len >= board_len {
            &mut space.product
        } else {
            &mut space.board
        };
        let longest_view = longest_series.remaining_artifacts.clone();

        // If the longest version list can't be split anymore, exit.
        if longest_view.len() <= 1 {
            return;
        }

        // Cut the length of the longest series in half, keeping the right half if the test passed
        // and keeping the left half if the test failed.
        let midpoint = longest_view.start + longest_view.len() / 2;
        if test_passed {
            longest_series.remaining_artifacts = (midpoint + 1)..longest_view.end;
        } else {
            longest_series.remaining_artifacts = longest_view.start..midpoint;
        }

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
        if let Some((good, bad)) = self.find_culprit(space, results) {
            let good_vas = &good.versioned_artifact_set;
            let bad_vas = &bad.versioned_artifact_set;

            let (good_artifact, bad_artifact) = if good_vas.platform != bad_vas.platform {
                (good_vas.platform.clone(), bad_vas.platform.clone())
            } else if good_vas.product != bad_vas.product {
                (good_vas.product.clone(), bad_vas.product.clone())
            } else {
                (good_vas.board.clone(), bad_vas.board.clone())
            };
            return BisectionStatus::CulpritFound(Box::new(good_artifact), Box::new(bad_artifact));
        }

        if space.iter_all_artifacts().all(|s| s.remaining_artifacts.len() <= 1) {
            return BisectionStatus::Exhausted;
        }

        BisectionStatus::Continue
    }

    fn find_culprit<'a>(
        &self,
        space: &SearchSpace,
        results: &'a Vec<crate::bisection_plan::StepResult>,
    ) -> Option<(&'a crate::bisection_plan::StepResult, &'a crate::bisection_plan::StepResult)>
    {
        // First, iterate through all test results to find a passing one.
        for good_candidate in results {
            if !good_candidate.test_passed {
                continue;
            }

            // Next, iterate through all results again to find a failing one.
            for bad_candidate in results {
                if bad_candidate.test_passed {
                    continue;
                }

                // Now we have a good and a bad candidate.
                let good_vas = &good_candidate.versioned_artifact_set;
                let bad_vas = &bad_candidate.versioned_artifact_set;

                // We count how many artifacts are different between the two sets.
                let mut diff_count = 0;
                if good_vas.platform != bad_vas.platform {
                    diff_count += 1;
                }
                if good_vas.product != bad_vas.product {
                    diff_count += 1;
                }
                if good_vas.board != bad_vas.board {
                    diff_count += 1;
                }

                // A culprit can only be identified if exactly one artifact has changed.
                // If more than one changed, we can't be sure which one caused the failure.
                if diff_count != 1 {
                    continue;
                }

                // Now, we determine which artifact was the one that changed
                // and check if the versions are adjacent in the search space.
                let (series, good_version, bad_version) = if good_vas.platform != bad_vas.platform {
                    (&space.platform, &good_vas.platform, &bad_vas.platform)
                } else if good_vas.product != bad_vas.product {
                    (&space.product, &good_vas.product, &bad_vas.product)
                } else {
                    (&space.board, &good_vas.board, &bad_vas.board)
                };

                // To check for adjacency, we find the index of each version
                // within the artifact's version list. The `position()` method
                // performs a linear search.
                let good_index = series.versions.iter().position(|v| v == good_version);
                let bad_index = series.versions.iter().position(|v| v == bad_version);

                // If both versions are found in the list...
                if let (Some(good_idx), Some(bad_idx)) = (good_index, bad_index) {
                    // ...we check if their indices are exactly 1 apart.
                    if (good_idx as isize - bad_idx as isize).abs() == 1 {
                        // If they are, we have found the culprit.
                        return Some((good_candidate, bad_candidate));
                    }
                }
            }
        }

        // If we loop through all pairs and don't find a culprit, return None.
        None
    }

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
    use crate::bisection_plan::StepResult;
    use crate::search_space::ArtifactVersionSeries;
    use crate::versioned_artifact_set::VersionedArtifactSet;
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

    fn create_mock_vas(platform_v: &str, product_v: &str, board_v: &str) -> VersionedArtifactSet {
        VersionedArtifactSet {
            platform: MOSIdentifier {
                artifact_type: ArtifactType::Platform,
                name: "platform".to_string(),
                version: platform_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            product: MOSIdentifier {
                artifact_type: ArtifactType::Product,
                name: "product".to_string(),
                version: product_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
            board: MOSIdentifier {
                artifact_type: ArtifactType::Board,
                name: "board".to_string(),
                version: board_v.to_string(),
                repository: "fuchsia".to_string(),
                cipd: None,
            },
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

    // ##################################################################
    // # 3. Termination Conditions (`should_continue` method)
    // ##################################################################

    #[test]
    /// Verifies that a culprit is correctly identified when a good and bad
    /// result are for adjacent artifact versions.
    fn test_should_continue_returns_culprit_found_on_adjacent_versions() {
        let space = create_mock_search_space(vec!["1", "2", "3", "4"], vec!["a"], vec!["x"]);
        let strategy = LongestDimensionStrategy;

        let results = vec![
            StepResult {
                versioned_artifact_set: create_mock_vas("2", "a", "x"),
                image_path: None,
                test_passed: true,
            },
            StepResult {
                versioned_artifact_set: create_mock_vas("3", "a", "x"),
                image_path: None,
                test_passed: false,
            },
        ];

        let status = strategy.should_continue(&space, &results);
        match status {
            BisectionStatus::CulpritFound(good, bad) => {
                assert_eq!(good.version, "2");
                assert_eq!(bad.version, "3");
            }
            _ => panic!("Expected CulpritFound status"),
        }
    }

    #[test]
    /// Verifies that a culprit is NOT identified if the differing versions
    /// are not adjacent in the search space.
    fn test_should_continue_does_not_find_culprit_for_non_adjacent_versions() {
        let space = create_mock_search_space(vec!["1", "2", "3", "4"], vec!["a"], vec!["x"]);
        let strategy = LongestDimensionStrategy;

        let results = vec![
            StepResult {
                versioned_artifact_set: create_mock_vas("2", "a", "x"),
                image_path: None,
                test_passed: true,
            },
            StepResult {
                versioned_artifact_set: create_mock_vas("4", "a", "x"),
                image_path: None,
                test_passed: false,
            },
        ];

        let status = strategy.should_continue(&space, &results);
        assert!(matches!(status, BisectionStatus::Continue));
    }

    #[test]
    /// Verifies that a culprit is NOT identified if the results differ by
    /// more than one artifact.
    fn test_should_continue_does_not_find_culprit_for_multiple_differences() {
        let space = create_mock_search_space(vec!["1", "2", "3", "4"], vec!["a", "b"], vec!["x"]);
        let strategy = LongestDimensionStrategy;

        let results = vec![
            StepResult {
                versioned_artifact_set: create_mock_vas("2", "a", "x"),
                image_path: None,
                test_passed: true,
            },
            StepResult {
                // Platform and Product are different
                versioned_artifact_set: create_mock_vas("3", "b", "x"),
                image_path: None,
                test_passed: false,
            },
        ];

        let status = strategy.should_continue(&space, &results);
        assert!(matches!(status, BisectionStatus::Continue));
    }

    #[test]
    /// Verifies that the status is `Exhausted` when all dimensions have been
    /// narrowed down to one or zero possibilities.
    fn test_should_continue_returns_exhausted_when_all_dimensions_are_len_one() {
        let mut space = create_mock_search_space(vec!["3"], vec!["b"], vec!["y"]);
        space.platform.remaining_artifacts = 0..1;
        space.product.remaining_artifacts = 0..1;
        space.board.remaining_artifacts = 0..1;
        let strategy = LongestDimensionStrategy;

        let status = strategy.should_continue(&space, &vec![]);
        assert!(matches!(status, BisectionStatus::Exhausted));
    }

    #[test]
    /// Verifies that the status is `Continue` when no other termination
    /// condition has been met.
    fn test_should_continue_returns_continue_by_default() {
        let space = create_mock_search_space(vec!["1", "2", "3"], vec!["a", "b"], vec!["x"]);
        let strategy = LongestDimensionStrategy;

        let status = strategy.should_continue(&space, &vec![]);
        assert!(matches!(status, BisectionStatus::Continue));
    }

    #[test]
    fn test_find_culprit() {
        let space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5"],
            vec!["a", "b", "c"],
            vec!["x", "y"],
        );
        let strategy = LongestDimensionStrategy {};
        let mut good_vas = create_mock_vas("1", "a", "x");
        good_vas.platform.version = "2".to_string();
        let mut bad_vas = create_mock_vas("1", "a", "x");
        bad_vas.platform.version = "3".to_string();

        let results = vec![
            StepResult {
                versioned_artifact_set: good_vas.clone(),
                image_path: None,
                test_passed: true,
            },
            StepResult {
                versioned_artifact_set: bad_vas.clone(),
                image_path: None,
                test_passed: false,
            },
        ];
        let culprit = strategy.find_culprit(&space, &results);
        assert!(culprit.is_some());

        // Not adjacent
        let mut bad_vas_not_adjacent = create_mock_vas("1", "a", "x");
        bad_vas_not_adjacent.platform.version = "4".to_string();
        let results_not_adjacent = vec![
            StepResult {
                versioned_artifact_set: good_vas.clone(),
                image_path: None,
                test_passed: true,
            },
            StepResult {
                versioned_artifact_set: bad_vas_not_adjacent,
                image_path: None,
                test_passed: false,
            },
        ];
        let culprit = strategy.find_culprit(&space, &results_not_adjacent);
        assert!(culprit.is_none());

        // Multiple diffs
        let mut bad_vas_multi_diff = create_mock_vas("1", "a", "x");
        bad_vas_multi_diff.platform.version = "3".to_string();
        bad_vas_multi_diff.product.version = "b".to_string();
        let results_multi_diff = vec![
            StepResult { versioned_artifact_set: good_vas, image_path: None, test_passed: true },
            StepResult {
                versioned_artifact_set: bad_vas_multi_diff,
                image_path: None,
                test_passed: false,
            },
        ];
        let culprit = strategy.find_culprit(&space, &results_multi_diff);
        assert!(culprit.is_none());
    }

    // ##################################################################
    // # 4. Estimation Logic (`estimate_total_steps` method)
    // ##################################################################

    #[test]
    /// Verifies that the step estimation logic is correct. The estimate is the
    /// sum of `ceil(log2(len))` for each dimension.
    fn test_estimate_total_steps_calculates_correctly() {
        // platform=8 (log2=3), product=5 (log2=2.32, ceil=3), board=1 (log2=0)
        // Total = 3 + 3 + 0 = 6
        let space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5", "6", "7", "8"],
            vec!["a", "b", "c", "d", "e"],
            vec!["x"],
        );
        let strategy = LongestDimensionStrategy;
        assert_eq!(strategy.estimate_total_steps(&space), 6);

        // platform=1, product=1, board=1 -> 0 steps
        let space_single = create_mock_search_space(vec!["1"], vec!["a"], vec!["x"]);
        assert_eq!(strategy.estimate_total_steps(&space_single), 0);

        // platform=2, product=2, board=2 -> 1+1+1 = 3 steps
        let space_two = create_mock_search_space(vec!["1", "2"], vec!["a", "b"], vec!["x", "y"]);
        assert_eq!(strategy.estimate_total_steps(&space_two), 3);
    }
}
