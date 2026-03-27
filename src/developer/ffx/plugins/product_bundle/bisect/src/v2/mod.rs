// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod assembly;
mod controller;
mod dimension;
mod mos;
mod search_space;
mod strategy;
mod testing;

pub use assembly::assemble;
pub use controller::Controller;
pub use dimension::Dimension;
pub use mos::get_search_space;
pub use search_space::SearchSpace;
pub use strategy::{SearchStrategy, StrategyState};
pub use testing::{prompt_for_manual_test, run_automated_test};
