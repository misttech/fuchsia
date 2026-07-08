// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Debug;
use std::ops::Deref;

use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

use super::NewPolicy;
use super::error::{ParseError, SerializeError, ValidateError};
use super::traits::{Parse, Serialize, Validate};
use selinux_policy_derive::{Parse, Serialize};

/// Cursor used to parse elements from the binary policy data.
#[derive(Debug)]
pub struct PolicyCursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> PolicyCursor<'a> {
    /// Creates a new [`PolicyCursor`] wrapping the supplied `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    /// Returns the current offset of the cursor.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Parses a type `T` that implements [`Parse`] from the cursor.
    pub fn parse<T: Parse>(&mut self) -> Result<T, ParseError> {
        T::parse(self)
    }

    /// Parses `count` contiguous elements of type `T` from the cursor.
    pub(super) fn parse_elements<T: Parse>(
        &mut self,
        count: usize,
    ) -> Result<Box<[T]>, ParseError> {
        let mut elements = Vec::with_capacity(count);
        for _ in 0..count {
            elements.push(self.parse()?);
        }
        Ok(elements.into_boxed_slice())
    }

    /// Reads a generic sized, zerocopyable type directly from the cursor.
    fn read<T: FromBytes + KnownLayout + Immutable + Unaligned>(
        &mut self,
    ) -> Result<T, ParseError> {
        let size = std::mem::size_of::<T>();
        let bytes = self.read_bytes(size)?;
        // Safe because T is FromBytes, Immutable, Unaligned and bytes is exactly `size` bytes.
        let value = T::read_from_bytes(bytes).map_err(|_| ParseError::MissingData {
            type_name: std::any::type_name::<T>(),
            type_size: size,
            num_bytes: bytes.len(),
        })?;
        Ok(value)
    }

    /// Reads a slice of `count` bytes from the cursor.
    pub(super) fn read_bytes(&mut self, count: usize) -> Result<&'a [u8], ParseError> {
        if self.offset + count > self.data.len() {
            return Err(ParseError::MissingData {
                type_name: "bytes",
                type_size: count,
                num_bytes: self.data.len() - self.offset,
            });
        }
        let bytes = &self.data[self.offset..self.offset + count];
        self.offset += count;
        Ok(bytes)
    }
}

impl Parse for u32 {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let val: le::U32 = cursor.read::<le::U32>()?;
        Ok(val.get())
    }
}

impl Serialize for u32 {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let val = le::U32::new(*self);
        writer.extend_from_slice(zerocopy::IntoBytes::as_bytes(&val));
        Ok(())
    }
}

impl Validate for u32 {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

/// Container representing a `u32` count followed by that many raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteArray {
    data: Box<[u8]>,
}

impl Deref for ByteArray {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl AsRef<[u8]> for ByteArray {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Parse for ByteArray {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let count = u32::parse(cursor)? as usize;
        let bytes = cursor.read_bytes(count)?;
        Ok(Self { data: bytes.to_vec().into_boxed_slice() })
    }
}

impl Serialize for ByteArray {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let count = self.data.len() as u32;
        count.serialize(writer)?;
        writer.extend_from_slice(&self.data);
        Ok(())
    }
}

impl Validate for ByteArray {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

/// Remaining unparsed bytes of the policy. NewPolicy parses only the first few
/// elements of the binary policy, retaining the trailing bytes both to allow
/// byte-for-byte re-serialization of the whole policy, and for the old policy
/// framework to use to parse the fields that have not yet been migrated. This
/// field will be removed once the migration is complete.
#[derive(Debug, Clone)]
pub(super) struct RemainingBytes {
    pub(super) bytes: std::sync::Arc<[u8]>,
}

impl Parse for RemainingBytes {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let rest = std::sync::Arc::from(&cursor.data[cursor.offset..]);
        cursor.offset = cursor.data.len();
        Ok(Self { bytes: rest })
    }
}

impl Serialize for RemainingBytes {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        writer.extend_from_slice(&self.bytes);
        Ok(())
    }
}

impl Validate for RemainingBytes {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

impl Parse for u64 {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let val: le::U64 = cursor.read::<le::U64>()?;
        Ok(val.get())
    }
}

impl Serialize for u64 {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let val = le::U64::new(*self);
        writer.extend_from_slice(zerocopy::IntoBytes::as_bytes(&val));
        Ok(())
    }
}

impl Validate for u64 {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

impl<T> Parse for std::marker::PhantomData<T> {
    fn parse(_cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        Ok(Self)
    }
}

impl<T> Serialize for std::marker::PhantomData<T> {
    fn serialize(&self, _writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        Ok(())
    }
}

impl<T> Validate for std::marker::PhantomData<T> {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

/// Sized array of elements of type `T`, prefixed by a `u32` count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Array<T> {
    elements: Box<[T]>,
}

impl<T> From<Vec<T>> for Array<T> {
    fn from(v: Vec<T>) -> Self {
        Self { elements: v.into_boxed_slice() }
    }
}

impl<T> Deref for Array<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.elements
    }
}

impl<T> AsRef<[T]> for Array<T> {
    fn as_ref(&self) -> &[T] {
        &self.elements
    }
}

impl<T: Parse> Parse for Array<T> {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let count = u32::parse(cursor)? as usize;
        let elements = cursor.parse_elements(count)?;
        Ok(Self { elements })
    }
}

impl<T: Serialize> Serialize for Array<T> {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let count = self.elements.len() as u32;
        count.serialize(writer)?;
        for element in self.elements.iter() {
            element.serialize(writer)?;
        }
        Ok(())
    }
}

impl<T: Validate> Validate for Array<T> {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        for element in self.elements.iter() {
            element.validate(policy)?;
        }
        Ok(())
    }
}

/// Symbol list header and associated array of items.
///
/// In the SELinux binary policy database, each symbol table is prefixed by a header
/// containing a `primary_names_count` field, followed by the array of items (which itself
/// is prefixed by an `items_count` field).
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize)]
pub struct SymbolArray<T> {
    /// The number of primary names in this array (excluding aliases).
    /// Included in the policy to allow allocation of index structures to be optimized.
    primary_names_count: u32,
    items: Array<T>,
}

impl<T> Deref for SymbolArray<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl<T: Validate> Validate for SymbolArray<T> {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        self.items.validate(policy)?;
        if self.primary_names_count > self.items.len() as u32 {
            return Err(ValidateError::InvalidPrimaryNamesCount {
                expected_at_most: self.items.len() as u32,
                found: self.primary_names_count,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_policy_cursor_parse_u32() {
        let data = [1, 0, 0, 0, 2, 0, 0, 0];
        let mut cursor = PolicyCursor::new(&data);

        let val1 = cursor.parse::<u32>().unwrap();
        assert_eq!(val1, 1);
        assert_eq!(cursor.offset, 4);

        let val2 = cursor.parse::<u32>().unwrap();
        assert_eq!(val2, 2);
        assert_eq!(cursor.offset, 8);
    }

    #[test]
    fn test_policy_cursor_parse_u64() {
        let data = [1, 0, 0, 0, 0, 0, 0, 0];
        let mut cursor = PolicyCursor::new(&data);
        let val = cursor.parse::<u64>().unwrap();
        assert_eq!(val, 1);
        assert_eq!(cursor.offset, 8);
    }

    #[test]
    fn test_policy_cursor_parse_elements() {
        let data = [1, 0, 0, 0, 2, 0, 0, 0];
        let mut cursor = PolicyCursor::new(&data);

        let elements = cursor.parse_elements::<u32>(2).unwrap();
        assert_eq!(elements.as_ref(), &[1, 2]);
        assert_eq!(cursor.offset, 8);
    }

    #[test]
    fn test_policy_cursor_missing_data_u32() {
        let data = [1, 0];
        let mut cursor = PolicyCursor::new(&data);

        let err = cursor.parse::<u32>().unwrap_err();
        assert!(matches!(
            err,
            ParseError::MissingData { type_name: "bytes", type_size: 4, num_bytes: 2 }
        ));
    }

    #[test]
    fn test_u32_serialize() {
        let val = 42u32;
        let mut writer = Vec::new();
        val.serialize(&mut writer).unwrap();
        assert_eq!(writer, [42, 0, 0, 0]);
    }

    #[test]
    fn test_u64_serialize() {
        let val = 42u64;
        let mut writer = Vec::new();
        val.serialize(&mut writer).unwrap();
        assert_eq!(writer, [42, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_policy_cursor_byte_array() {
        let data = [4, 0, 0, 0, 5, 6, 7, 8];
        let mut cursor = PolicyCursor::new(&data);

        let array = cursor.parse::<ByteArray>().unwrap();
        assert_eq!(array.as_ref(), &[5, 6, 7, 8]);
        assert_eq!(cursor.offset, 8);
    }

    #[test]
    fn test_policy_cursor_byte_array_missing_data() {
        let data = [5, 0, 0, 0, 6, 7, 8];
        let mut cursor = PolicyCursor::new(&data);

        let err = cursor.parse::<ByteArray>().unwrap_err();
        assert!(matches!(
            err,
            ParseError::MissingData { type_name: "bytes", type_size: 5, num_bytes: 3 }
        ));
    }

    #[test]
    fn test_byte_array_serialize() {
        let array = ByteArray { data: vec![5, 6, 7, 8].into_boxed_slice() };
        let mut writer = Vec::new();
        array.serialize(&mut writer).unwrap();
        assert_eq!(writer, [4, 0, 0, 0, 5, 6, 7, 8]);
    }

    #[test]
    fn test_remaining_bytes_parse_and_serialize() {
        let data = [1, 0, 0, 0, 9, 9, 9];
        let mut cursor = PolicyCursor::new(&data);

        // Parse first u32
        let val = cursor.parse::<u32>().unwrap();
        assert_eq!(val, 1);

        // Parse remaining bytes
        let remaining = cursor.parse::<RemainingBytes>().unwrap();
        assert_eq!(remaining.bytes.as_ref(), &[9, 9, 9]);

        // Serialize remaining bytes
        let mut writer = Vec::new();
        remaining.serialize(&mut writer).unwrap();
        assert_eq!(writer, [9, 9, 9]);
    }

    #[test]
    fn test_array_parse_and_serialize() {
        let data = [2, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0];
        let mut cursor = PolicyCursor::new(&data);

        let array = cursor.parse::<Array<u32>>().unwrap();
        assert_eq!(array.as_ref(), &[10, 20]);
        assert_eq!(cursor.offset, 12);

        let mut writer = Vec::new();
        array.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }

    #[test]
    fn test_symbol_array_parse_and_serialize() {
        // primary_names_count = 5, array count = 2, elements = [10, 20]
        let data = [5, 0, 0, 0, 2, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0];
        let mut cursor = PolicyCursor::new(&data);

        let sym_array = cursor.parse::<SymbolArray<u32>>().unwrap();
        assert_eq!(sym_array.primary_names_count, 5);
        assert_eq!(sym_array.as_ref(), &[10, 20]);
        assert_eq!(cursor.offset, 16);

        let mut writer = Vec::new();
        sym_array.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }
}
