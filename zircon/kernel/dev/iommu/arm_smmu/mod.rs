// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMU driver modules shared across SMMUv2 and SMMUv3.
//!
//! The driver is organized into:
//! - [`common`]: Architecture-version-agnostic VMSAv8-64 page table, address space, page cache,
//!   boot mode parsing, and `SmmuEnv` environment abstractions reusable by both SMMUv2 and SMMUv3.
//! - [`v2`]: ARM SMMUv2 controller, context bank, stream matching register group, BTI, PMT, and
//!   register definitions.
//! - [`unit_tests`]: Native Rust component unit tests shared between the host test binary and the
//!   in-kernel `#[ktest]` suite.
//!
//! Submodules are marked `pub` so that the standalone host test crate (`tests/host_lib.rs`) can
//! mount and test their internals directly.

pub mod common;
#[cfg(any(ktest, not(is_kernel)))]
pub mod unit_tests;
pub mod v2;
