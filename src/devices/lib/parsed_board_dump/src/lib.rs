// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `parsed_board_dump` is the foundational library defining the Abstract Syntax Tree (AST)
//! and serialization formats for Fuchsia hardware board topologies.
//!
//! It serves as the standard, canonical intermediate representation used across
//! Board DML compilers, validators, Inspect dumpers, Devicetree parsers, and host verification tools.

pub mod ast;
pub mod parse;

pub use ast::*;
pub use parse::*;
