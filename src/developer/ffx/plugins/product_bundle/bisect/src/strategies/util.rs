// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains shared utility functions used by multiple bisection strategies.

use crate::bisection_plan::StepResult;
use crate::search_space::{BisectionStatus, SearchSpace};

/// A common implementation for finding the culprit between a good and bad build.
///
/// A culprit is found if there is a pair of test results (one pass, one fail)
/// that differ by exactly one artifact, and the versions of that artifact are
/// adjacent in the search space.
pub(super) fn find_culprit<'a>(
    space: &SearchSpace,
    results: &'a Vec<StepResult>,
) -> Option<(&'a StepResult, &'a StepResult)> {
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

/// A common implementation for determining if the bisection should continue.
///
/// The bisection terminates if a culprit is found or if the search space
/// has been exhausted (all dimensions have 1 or 0 artifacts left).
pub(super) fn should_continue(space: &SearchSpace, results: &Vec<StepResult>) -> BisectionStatus {
    if let Some((good, bad)) = find_culprit(space, results) {
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

/// A helper function to perform the core bisection logic on the single longest
/// dimension of the search space.
pub(super) fn halve_longest_dimension(space: &mut SearchSpace, test_passed: bool) {
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

    // If the longest version list can't be split anymore, there's nothing to do.
    if longest_series.remaining_artifacts.len() <= 1 {
        return;
    }

    let view = longest_series.remaining_artifacts.clone();
    let midpoint = view.start + view.len() / 2;

    // Cut the length of the longest series in half, keeping the right half if the test passed
    // and keeping the left half if the test failed.
    if test_passed {
        longest_series.remaining_artifacts = (midpoint + 1)..view.end;
    } else {
        longest_series.remaining_artifacts = view.start..midpoint;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    // # Termination Conditions (`should_continue` method)
    // ##################################################################

    #[test]
    /// Verifies that a culprit is correctly identified when a good and bad
    /// result are for adjacent artifact versions.
    fn test_should_continue_returns_culprit_found_on_adjacent_versions() {
        let space = create_mock_search_space(vec!["1", "2", "3", "4"], vec!["a"], vec!["x"]);

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

        let status = should_continue(&space, &results);
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

        let status = should_continue(&space, &results);
        assert!(matches!(status, BisectionStatus::Continue));
    }

    #[test]
    /// Verifies that a culprit is NOT identified if the results differ by
    /// more than one artifact.
    fn test_should_continue_does_not_find_culprit_for_multiple_differences() {
        let space = create_mock_search_space(vec!["1", "2", "3", "4"], vec!["a", "b"], vec!["x"]);

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

        let status = should_continue(&space, &results);
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

        let status = should_continue(&space, &vec![]);
        assert!(matches!(status, BisectionStatus::Exhausted));
    }

    #[test]
    /// Verifies that the status is `Continue` when no other termination
    /// condition has been met.
    fn test_should_continue_returns_continue_by_default() {
        let space = create_mock_search_space(vec!["1", "2", "3"], vec!["a", "b"], vec!["x"]);

        let status = should_continue(&space, &vec![]);
        assert!(matches!(status, BisectionStatus::Continue));
    }

    #[test]
    fn test_find_culprit() {
        let space = create_mock_search_space(
            vec!["1", "2", "3", "4", "5"],
            vec!["a", "b", "c"],
            vec!["x", "y"],
        );
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
        let culprit = find_culprit(&space, &results);
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
        let culprit = find_culprit(&space, &results_not_adjacent);
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
        let culprit = find_culprit(&space, &results_multi_diff);
        assert!(culprit.is_none());
    }
}
