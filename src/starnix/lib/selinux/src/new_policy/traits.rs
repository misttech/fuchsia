// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::NewPolicy;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::PolicyCursor;

/// Trait for types that can be parsed from a [`PolicyCursor`].
pub trait Parse: Sized {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError>;
}

/// Trait for types that can be serialized into a byte vector.
pub trait Serialize {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError>;
}

/// Trait for types that can be validated against the parsed policy.
pub trait Validate {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError>;
}
