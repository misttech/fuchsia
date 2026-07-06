// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
use super::Parse;
#[cfg(test)]
use super::error::ParseError;
#[cfg(test)]
use super::error::ValidateError;
#[cfg(test)]
use super::parser::PolicyCursor;
#[cfg(test)]
use super::{
    Array, Counted, PolicyValidationContext, Validate, ValidateArray, array_type,
    array_type_validate_deref_both,
};

#[cfg(test)]
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

#[cfg(test)]
use crate::new_policy::HandleUnknown;

pub(super) const SELINUX_MAGIC: u32 = 0xf97cff8c;

pub(super) const POLICYDB_STRING_MAX_LENGTH: u32 = 32;
pub(super) const POLICYDB_SIGNATURE: &[u8] = b"SE Linux";

pub(super) const POLICYDB_VERSION_MIN: u32 = 30;
pub const POLICYDB_VERSION_MAX: u32 = 33;

pub(super) const CONFIG_MLS_FLAG: u32 = 1;
pub(super) const CONFIG_HANDLE_UNKNOWN_REJECT_FLAG: u32 = 1 << 1;
pub(super) const CONFIG_HANDLE_UNKNOWN_ALLOW_FLAG: u32 = 1 << 2;
pub(super) const CONFIG_HANDLE_UNKNOWN_MASK: u32 =
    CONFIG_HANDLE_UNKNOWN_REJECT_FLAG | CONFIG_HANDLE_UNKNOWN_ALLOW_FLAG;

#[cfg(test)]
#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct Magic(le::U32);

#[cfg(test)]
impl Validate for Magic {
    type Error = ValidateError;

    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        let found_magic = self.0.get();
        if found_magic != SELINUX_MAGIC {
            Err(ValidateError::InvalidMagic { found_magic })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
array_type!(Signature, SignatureMetadata, u8);

#[cfg(test)]
array_type_validate_deref_both!(Signature);

#[cfg(test)]
impl ValidateArray<SignatureMetadata, u8> for Signature {
    type Error = ValidateError;

    fn validate_array(
        _context: &PolicyValidationContext,
        _metadata: &SignatureMetadata,
        items: &[u8],
    ) -> Result<(), Self::Error> {
        if items != POLICYDB_SIGNATURE {
            Err(ValidateError::InvalidSignature { found_signature: items.to_owned() })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct SignatureMetadata(le::U32);

#[cfg(test)]
impl Validate for SignatureMetadata {
    type Error = ValidateError;

    /// [`SignatureMetadata`] has no constraints.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        let found_length = self.0.get();
        if found_length > POLICYDB_STRING_MAX_LENGTH {
            Err(ValidateError::InvalidSignatureLength { found_length })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
impl Counted for SignatureMetadata {
    fn count(&self) -> u32 {
        self.0.get()
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct Config {
    #[allow(dead_code)]
    config: le::U32,
}

#[cfg(test)]
impl Parse for Config {
    type Error = ParseError;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let (config, tail) = PolicyCursor::parse::<le::U32>(bytes)?;

        let found_config = config.get();
        if found_config & CONFIG_MLS_FLAG == 0 {
            return Err(ParseError::ConfigMissingMlsFlag { found_config });
        }
        let _ = try_handle_unknown_fom_config(found_config)?;

        Ok((Self { config }, tail))
    }
}

#[cfg(test)]
impl Validate for Config {
    type Error = anyhow::Error;

    /// All validation for [`Config`] is necessary to parse it correctly. No additional validation
    /// required.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
fn try_handle_unknown_fom_config(config: u32) -> Result<HandleUnknown, ParseError> {
    match config & CONFIG_HANDLE_UNKNOWN_MASK {
        CONFIG_HANDLE_UNKNOWN_ALLOW_FLAG => Ok(HandleUnknown::Allow),
        CONFIG_HANDLE_UNKNOWN_REJECT_FLAG => Ok(HandleUnknown::Reject),
        0 => Ok(HandleUnknown::Deny),
        _ => Err(ParseError::InvalidHandleUnknownConfigurationBits {
            masked_bits: (config & CONFIG_HANDLE_UNKNOWN_MASK),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::{PolicyCursor, PolicyData};
    use super::super::testing::as_parse_error;

    use super::*;

    // TODO: Run this test over `validate()`.
    #[test]
    fn no_magic() {
        let mut bytes = [SELINUX_MAGIC.to_le_bytes().as_slice()].concat();
        // One byte short of magic.
        bytes.pop();
        let data = PolicyData::from(bytes);
        assert_eq!(
            Err(ParseError::MissingData {
                type_name: "selinux_lib_test::policy::metadata::Magic",
                type_size: 4,
                num_bytes: 3
            }),
            PolicyCursor::parse::<Magic>(PolicyCursor::new(&data)),
        );
    }

    #[test]
    fn missing_signature() {
        let bytes = [(1 as u32).to_le_bytes().as_slice()].concat();
        let data = PolicyData::from(bytes);
        match Signature::parse(PolicyCursor::new(&data)).err().map(as_parse_error) {
            Some(ParseError::MissingData { type_name: "u8", type_size: 1, num_bytes: 0 }) => {}
            parse_err => {
                assert!(false, "Expected Some(MissingData...), but got {:?}", parse_err);
            }
        }
    }

    #[test]
    fn config_missing_mls_flag() {
        let bytes = [(!CONFIG_MLS_FLAG).to_le_bytes().as_slice()].concat();
        let data = PolicyData::from(bytes);
        match Config::parse(PolicyCursor::new(&data)).err() {
            Some(ParseError::ConfigMissingMlsFlag { .. }) => {}
            parse_err => {
                assert!(false, "Expected Some(ConfigMissingMlsFlag...), but got {:?}", parse_err);
            }
        }
    }

    #[test]
    fn invalid_handle_unknown() {
        let bytes = [(CONFIG_MLS_FLAG
            | CONFIG_HANDLE_UNKNOWN_ALLOW_FLAG
            | CONFIG_HANDLE_UNKNOWN_REJECT_FLAG)
            .to_le_bytes()
            .as_slice()]
        .concat();
        let data = PolicyData::from(bytes);
        assert_eq!(
            Some(ParseError::InvalidHandleUnknownConfigurationBits {
                masked_bits: CONFIG_HANDLE_UNKNOWN_ALLOW_FLAG | CONFIG_HANDLE_UNKNOWN_REJECT_FLAG
            }),
            Config::parse(PolicyCursor::new(&data)).err()
        );
    }
}
