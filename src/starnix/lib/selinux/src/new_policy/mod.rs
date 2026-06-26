// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub(super) mod error;
pub(super) mod metadata;
pub(super) mod parser;
pub(super) mod traits;

use selinux_policy_derive::{Parse, Serialize, Validate};

use error::{ParseError, SerializeError, ValidateError};
pub use metadata::HandleUnknown;
use metadata::{Config, Counts, Magic, PolicyVersion, Signature};
use parser::{PolicyCursor, RemainingBytes};
use traits::{Parse, Serialize, Validate};

/// Top-level [`NewPolicy`] structure that parses the first few fields
/// and stores the rest in [`Self::rest`] to allow round-trip testing.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct NewPolicy {
    magic: Magic,
    signature: Signature,
    version: PolicyVersion,
    config: Config,
    counts: Counts,
    rest: RemainingBytes,
}

impl NewPolicy {
    /// Parses a [`NewPolicy`] from the raw binary data.
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        let mut cursor = PolicyCursor::new(data);
        cursor.parse()
    }

    /// Validates the parsed policy.
    pub fn validate(&self) -> Result<(), ValidateError> {
        Validate::validate(self, self)
    }

    /// Returns the policy version.
    pub fn policy_version(&self) -> u32 {
        self.version.get()
    }

    /// Returns the [`HandleUnknown`] configuration.
    pub fn handle_unknown(&self) -> HandleUnknown {
        self.config.handle_unknown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_real_policy_roundtrip() {
        let policy_bytes = include_bytes!("../../testdata/policies/selinux_testsuite");
        let new_policy = NewPolicy::parse(policy_bytes).unwrap();
        new_policy.validate().unwrap();

        // Verify metadata basics
        assert!(new_policy.policy_version() >= 30);
        assert_eq!(new_policy.handle_unknown(), HandleUnknown::Allow);

        // Verify 100% byte-for-byte roundtrip fidelity
        let mut serialized = Vec::new();
        new_policy.serialize(&mut serialized).unwrap();
        assert_eq!(serialized.len(), policy_bytes.len());
        assert_eq!(serialized, policy_bytes);
    }
}
