// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains different strategies for bisecting product bundles.

pub(crate) mod all_dimensions;
pub(crate) mod longest_dimension;
mod util;

use crate::bisection_plan::StepResult;
use crate::search_space::{BisectionStatus, SearchSpace};
use all_dimensions::AllDimensionsStrategy;
use longest_dimension::LongestDimensionStrategy;
use serde::{Deserialize, Serialize};

/// A trait for bisection search strategies.
pub trait SearchStrategy {
    /// Updates the search space based on the result of a test.
    fn update(&self, space: &mut SearchSpace, test_passed: bool);

    /// Determines whether the bisection should continue, has found a culprit,
    /// or has exhausted the search space.
    fn should_continue(&self, space: &SearchSpace, results: &Vec<StepResult>) -> BisectionStatus;

    /// Estimates the total number of steps required for the bisection.
    fn estimate_total_steps(&self, space: &SearchSpace) -> usize;
}

/// An enumeration of available bisection search strategies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Strategy {
    /// Bisect the longest dimension first.
    LongestDimension,
    /// Bisect all dimensions simultaneously.
    AllDimensions,
}

impl Strategy {
    /// Returns a dynamically dispatchable trait object for the selected search strategy.
    pub fn as_dyn(&self) -> &dyn SearchStrategy {
        match self {
            Strategy::LongestDimension => &LongestDimensionStrategy,
            Strategy::AllDimensions => &AllDimensionsStrategy,
        }
    }
}

/// Returns the appropriate `Strategy` enum variant based on the command-line arguments.
pub fn get_strategy(strategy: ffx_product_bundle_bisect_args::Strategy) -> Strategy {
    match strategy {
        ffx_product_bundle_bisect_args::Strategy::LongestDimension => Strategy::LongestDimension,
        ffx_product_bundle_bisect_args::Strategy::AllDimensions => Strategy::AllDimensions,
    }
}
