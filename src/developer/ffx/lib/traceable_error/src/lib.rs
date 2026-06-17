// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! Deterministic Error Tracing Architecture (`traceable_error`)
//!
//! This crate provides the foundational traits and compile-time hashing mechanisms
//! required to establish a deterministic, traceable error hierarchy across distributed
//! systems and multi-layered software architectures (such as Fuchsia and `ffx`).
//!
//! ## Overview
//!
//! When dealing with deeply nested software stacks or distributed IPC boundaries
//! (e.g. FIDL, Overnet), errors often undergo type erasure or stringification. This crate
//! establishes a mechanism where each distinct error variant across independent crates
//! is assigned a stable 32-bit layer code composed of:
//! - **24-bit Crate Hash**: Generated at compile time from the crate's package name via FNV-1a.
//! - **8-bit Variant ID**: Discriminant assigned by the procedural macro based on variant declaration order.
//!
//! By chaining these layer codes chronologically, diagnostic systems can reconstruct the exact
//! trajectory of a failure without relying on brittle string parsing or runtime type metadata.

/// Defines an error that can be deterministically traced through a distributed architecture.
///
/// Implementations of this trait (typically derived automatically via `#[derive(TraceableError)]`)
/// are capable of interrogating their underlying causal chain and reporting a unified chronological
/// history of layer codes.
///
/// Each layer code is represented by a structured `String` identifier:
/// `format!("{crate_name}::{enum_name}::{variant_name}")`.
///
/// This structured layout facilitates highly readable failure trajectory reconstruction
/// across distributed IPC boundaries and dynamic crate boundaries.
pub trait TraceableError {
    /// Returns this specific layer's string identifier (format: CrateName::EnumName::EnumValue).
    fn layer_code(&self) -> String;

    /// Recursively interrogates underlying error sources to build the chronological array of layer codes.
    ///
    /// The resulting vector is ordered from outermost (most recent) layer to innermost (root cause).
    fn chain_codes(&self) -> Vec<String>;

    /// Formats the layer code vector into a standardized diagnostic string (e.g., `"Crate1::Enum1::Val1-Crate2::Enum2::Val2"`).
    fn diagnostic_code(&self) -> String {
        self.chain_codes().join("-")
    }
}

impl TraceableError for anyhow::Error {
    fn layer_code(&self) -> String {
        "anyhow".to_string()
    }

    fn chain_codes(&self) -> Vec<String> {
        vec![self.layer_code()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyError;
    impl TraceableError for DummyError {
        fn layer_code(&self) -> String {
            "DummyError".to_string()
        }
        fn chain_codes(&self) -> Vec<String> {
            vec![self.layer_code()]
        }
    }

    #[test]
    fn test_traceable_error() {
        let _err = DummyError;
    }

    #[test]
    fn test_anyhow_traceable() {
        let err = anyhow::anyhow!("boom");
        assert_eq!(err.chain_codes().len(), 1);
        assert_eq!(err.chain_codes()[0], "anyhow");
    }
}
