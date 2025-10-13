// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::search_space::{BisectionStatus, SearchSpace};
use crate::strategies::longest_dimension::LongestDimensionStrategy;
use ffx_product_bisect_args as args;
use serde::{Deserialize, Serialize};

pub mod longest_dimension;

/// A trait for defining bisection search strategies.
pub trait SearchStrategy {
    /// Given the current search space and a test result, update the search ranges
    /// to narrow down the possibilities.
    fn update(&self, space: &mut SearchSpace, test_passed: bool);

    /// Determines if the bisection should continue, and returns the current status.
    fn should_continue(
        &self,
        space: &SearchSpace,
        results: &Vec<crate::bisection_plan::StepResult>,
    ) -> BisectionStatus;

    /// A culprit is found when a known-good and known-bad build are adjacent
    /// in the search space.
    fn find_culprit<'a>(
        &self,
        space: &SearchSpace,
        results: &'a Vec<crate::bisection_plan::StepResult>,
    ) -> Option<(&'a crate::bisection_plan::StepResult, &'a crate::bisection_plan::StepResult)>;

    /// Estimates the total number of steps required for the bisection.
    fn estimate_total_steps(&self, space: &SearchSpace) -> usize;
}

#[derive(Serialize, Deserialize)]
pub enum Strategy {
    LongestDimension(LongestDimensionStrategy),
}

impl Strategy {
    pub fn as_dyn(&self) -> &dyn SearchStrategy {
        match self {
            Strategy::LongestDimension(s) => s,
        }
    }
}

pub fn get_strategy(strategy_name: args::Strategy) -> Strategy {
    match strategy_name {
        args::Strategy::LongestDimension => Strategy::LongestDimension(LongestDimensionStrategy),
    }
}
