// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::SearchSpace;
use crate::strategy::{SearchStrategy, StrategyState};
use anyhow::{Context, Result};
use assembly_artifact_cache::{MOSIdentifier, Slot};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufReader;

/// A struct that holds the serialized state of the bisection process.
#[derive(Serialize, Deserialize)]
struct Plan {
    space: SearchSpace,
    strategy: SearchStrategy,
    step_count: usize,
}

/// The Controller orchestrates the bisection state machine, saving and restoring
/// state to disk so that long-running, interruptible tasks (like flashing devices)
/// can resume where they left off.
pub struct Controller<TestFn, PrintFn> {
    /// The search space containing all dimensions and versions.
    pub space: SearchSpace,
    /// The bisection strategy state machine.
    pub strategy: SearchStrategy,
    /// The path to the JSON file where state is persisted.
    pub plan: Utf8PathBuf,
    /// The asynchronous function that performs the actual testing.
    test_fn: TestFn,
    /// The function that reports the current status of the bisection.
    print_fn: PrintFn,
    /// The slot to use when constructing MOSIdentifiers.
    slot: Slot,
    /// The number of bisection steps taken so far.
    pub step_count: usize,
}

impl<TestFn, TestFut, PrintFn> Controller<TestFn, PrintFn>
where
    TestFn: FnMut(Vec<MOSIdentifier>) -> TestFut,
    TestFut: std::future::Future<Output = Result<bool>>,
    PrintFn: FnMut(&str),
{
    /// Creates a new Controller. If a saved plan exists at `plan`, it will be
    /// loaded. Otherwise, a new plan will be created from the provided `space`.
    pub fn new(
        mut space: SearchSpace,
        plan: Utf8PathBuf,
        slot: Slot,
        test_fn: TestFn,
        print_fn: PrintFn,
    ) -> Result<Self> {
        let mut strategy = SearchStrategy::new();
        let mut step_count = 0;

        if plan.is_file() {
            // Load existing state
            let file = File::open(&plan)
                .with_context(|| format!("Failed to open bisection plan at {}", plan))?;
            let reader = BufReader::new(file);
            let saved_plan: Plan = serde_json::from_reader(reader)
                .with_context(|| format!("Failed to parse bisection plan at {}", plan))?;

            space = saved_plan.space;
            strategy = saved_plan.strategy;
            step_count = saved_plan.step_count;
        } else {
            // Initialize new state
            if space.is_phase1_complete() {
                strategy.state = StrategyState::Phase2Isolation { current_dim_idx: 0 };
                strategy.advance_isolation_if_needed(&space);
            }
        }

        let controller = Self { space, strategy, plan, test_fn, print_fn, slot, step_count };

        // Ensure the initial state (or freshly loaded state) is saved.
        controller.save_plan().context("Failed to save initial bisection plan")?;

        Ok(controller)
    }

    /// Runs the bisection loop, yielding to the async `test_fn` for each combination
    /// and persisting state after each step. Returns the final StrategyState.
    pub async fn run(&mut self) -> Result<StrategyState> {
        (self.print_fn)("Starting Multi-dimensional bisection...\n\n");
        self.print_status();

        while let Some(indices) = self.strategy.next_combination(&self.space) {
            self.step_count += 1;

            let mut combination_strings = Vec::new();
            let combination: Vec<MOSIdentifier> = indices
                .iter()
                .enumerate()
                .map(|(i, &idx)| {
                    let dim = &self.space.dimensions[i];
                    combination_strings.push(format!("\"{}\"", dim.versions[idx]));
                    dim.get_mos_identifier(idx, self.slot)
                })
                .collect();

            let message = format!(
                "\nStep {}: Testing combination: [{}]",
                self.step_count,
                combination_strings.join(", ")
            );
            (self.print_fn)(&message);

            // Yield to the caller-provided async testing function
            let pass = (self.test_fn)(combination)
                .await
                .context("The provided testing function encountered an error during bisection")?;

            (self.print_fn)(&format!("Result: {}\n", if pass { "PASS" } else { "FAIL" }));

            // Update the state machine based on the result
            self.strategy.apply_result(&mut self.space, pass);
            self.save_plan().context("Failed to save bisection plan after step")?;
            self.print_status();

            // Check if we reached a terminal state
            if matches!(
                self.strategy.state,
                StrategyState::Resolved { .. } | StrategyState::Unresolved
            ) {
                break;
            }
        }

        let mut summary =
            String::from("\n=============================================\nBisection Complete!\n");
        match self.strategy.state {
            StrategyState::Resolved { dim_idx, low_idx, high_idx } => {
                let dim = &self.space.dimensions[dim_idx];
                summary.push_str(&format!("Root cause isolated to dimension: {}\n", dim.name));
                summary.push_str(&format!("Last known good version: {}\n", dim.versions[low_idx]));
                summary.push_str(&format!("First known bad version: {}\n", dim.versions[high_idx]));
            }
            StrategyState::Unresolved => {
                summary.push_str("Could not isolate a single root cause.\n");
                summary.push_str(
                    "The failure may involve multiple dimensions or bad initial boundaries.\n",
                );
            }
            _ => {}
        }
        summary.push_str("=============================================\n");
        (self.print_fn)(&summary);

        Ok(self.strategy.state.clone())
    }

    /// Formats and prints the current status of the bisection state machine.
    pub fn print_status(&mut self) {
        let mut message = String::new();
        message.push_str("Current Search Space Status:\n");
        for dim in &self.space.dimensions {
            let status = if dim.high == dim.low {
                format!("Pinned to [{}]", dim.versions[dim.low])
            } else if dim.high == dim.low + 1 {
                format!(
                    "Narrowed -> Good: [{}], Bad: [{}]",
                    dim.versions[dim.low], dim.versions[dim.high]
                )
            } else {
                format!(
                    "Active search between [{}] and [{}] ({} versions)",
                    dim.versions[dim.low],
                    dim.versions[dim.high],
                    dim.high - dim.low + 1
                )
            };
            message.push_str(&format!("  {:<10}: {}\n", dim.name, status));
        }

        let phase_str = match &self.strategy.state {
            StrategyState::Phase1Bisection => "Phase 1 (Multi-Axis Bisection)",
            StrategyState::Phase2Isolation { .. } => "Phase 2 (Isolating Root Cause)",
            StrategyState::Resolved { .. } => "Finished (Resolved)",
            StrategyState::Unresolved => "Finished (Unresolved)",
        };
        message.push_str(&format!("  Strategy Mode: {}\n", phase_str));

        (self.print_fn)(&message);
    }

    /// Serializes the current search space and strategy state to the plan file.
    fn save_plan(&self) -> Result<()> {
        let saved_plan = Plan {
            space: self.space.clone(),
            strategy: self.strategy.clone(),
            step_count: self.step_count,
        };

        let json_string = serde_json::to_string_pretty(&saved_plan)
            .context("Failed to serialize bisection plan to JSON")?;

        if let Some(parent) = self.plan.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directories for {}", self.plan)
            })?;
        }

        fs::write(&self.plan, json_string)
            .with_context(|| format!("Failed to write bisection plan to {}", self.plan))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimension::Dimension;
    use assembly_artifact_cache::ArtifactType;
    use tempfile::tempdir;

    #[test]
    fn test_controller_run_and_save_restore() {
        futures_lite::future::block_on(async move {
            let dir = tempdir().unwrap();
            let plan = Utf8PathBuf::from_path_buf(dir.path().join("plan.json")).unwrap();

            let dimensions = vec![
                Dimension::new(
                    "Platform",
                    ArtifactType::Platform,
                    "fuchsia",
                    vec!["1".into(), "2".into(), "3".into(), "4".into()],
                ),
                Dimension::new(
                    "Product",
                    ArtifactType::Product,
                    "fuchsia",
                    vec!["1".into(), "2".into(), "3".into()],
                ),
                Dimension::new(
                    "Board",
                    ArtifactType::Board,
                    "fuchsia",
                    vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into()],
                ),
            ];

            let space = SearchSpace::new(dimensions.clone());

            // First run: only do one step, then "crash" (drop the controller)
            {
                let test_fn = |comb: Vec<MOSIdentifier>| async move {
                    let board = &comb[2];
                    Ok(board.version != "4" && board.version != "5")
                };

                let mut controller =
                    Controller::new(space.clone(), plan.clone(), Slot::A, test_fn, |_| {}).unwrap();

                // Manually simulate exactly one step of the run loop
                let indices = controller.strategy.next_combination(&controller.space).unwrap();
                let combination: Vec<MOSIdentifier> = indices
                    .iter()
                    .enumerate()
                    .map(|(i, &idx)| {
                        controller.space.dimensions[i].get_mos_identifier(idx, Slot::A)
                    })
                    .collect();

                // The midpoint for Board(5) is idx 2 ("3"), which passes.
                let pass = combination[2].version != "4" && combination[2].version != "5";

                controller.strategy.apply_result(&mut controller.space, pass);
                controller.save_plan().unwrap();
            }

            // Second run: restore from disk and verify state was preserved
            {
                let test_fn = |comb: Vec<MOSIdentifier>| async move {
                    // Board changing from b3 -> b4 is the culprit
                    let board = &comb[2];
                    Ok(board.version != "4" && board.version != "5")
                };

                let mut controller =
                    Controller::new(space, plan.clone(), Slot::A, test_fn, |_| {}).unwrap();

                // The platform should have been narrowed to the UPPER half
                // (low=1) because the first test PASSED
                assert_eq!(controller.space.dimensions[0].low, 1);
                assert_eq!(controller.space.dimensions[0].high, 3);

                let result = controller.run().await.unwrap();

                // Ensure it eventually finds the correct culprit (Board 3 -> 4)
                if let StrategyState::Resolved { dim_idx, low_idx, high_idx } = result {
                    assert_eq!(dim_idx, 2);
                    assert_eq!(low_idx, 2);
                    assert_eq!(high_idx, 3);
                } else {
                    panic!("Did not resolve correctly, ended at {:?}", result);
                }
            }
        });
    }
}
