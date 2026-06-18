// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module implements a zero-copy/optimized byte representation for keys in Fxfs.
//!
//! Its goal is to facilitate lexicographically comparable key serialization, permitting optimal
//! byte representations when performing operations across persistent layer structures in LSM trees.

use crate::serialized_types::varint::{self, Buffer};
use anyhow::{Error, ensure};
use std::cmp;

/// Evaluates comparison ordering natively between two serialized byte arrays.
pub fn compare_keys(a: &[u8], b: &[u8]) -> Result<cmp::Ordering, Error> {
    ensure!(a.len() >= 2, "Key data too short");
    let len_a = u16::from_be_bytes(a[..2].try_into().unwrap()) as usize;
    ensure!(b.len() >= 2, "Key data too short");
    let len_b = u16::from_be_bytes(b[..2].try_into().unwrap()) as usize;
    ensure!(2 + len_a <= a.len(), "Key length exceeds buffer");
    let data_a = &a[2..2 + len_a];
    ensure!(2 + len_b <= b.len(), "Key length exceeds buffer");
    let data_b = &b[2..2 + len_b];
    Ok(data_a.cmp(data_b))
}

/// Serializes keys sequentially into binary format suitable for lexicographical comparisons.
///
/// The key layout contains:
/// - A 2-byte Big-Endian length prefix specifying the size of the serialized data in bytes.
/// - The encoded variable-length binary key payload.
pub struct KeySerializer<'a, B: Buffer> {
    buffer: &'a mut B,
    done: bool,
    base: Option<u64>,
    start_pos: usize,
}

impl<'a, B: Buffer> KeySerializer<'a, B> {
    /// Creates a new `KeySerializer` attached to a persistent storage buffer.
    pub fn new(buffer: &'a mut B, base: Option<u64>) -> Self {
        let start_pos = buffer.as_ref().len();
        // Placeholder for the two-byte length prefix.
        buffer.put(&[0, 0]);
        Self { buffer, done: false, base, start_pos }
    }

    /// Writes an order-preserving varint encoded 64-bit payload to buffer.
    /// If a base was provided and this is the first u64, it writes the delta.
    pub fn write_u64(&mut self, v: u64) {
        debug_assert!(!self.done);
        if let Some(base) = self.base.take() {
            assert_eq!(
                self.buffer.as_ref().len(),
                self.start_pos + 2,
                "write_u64 with base must be the first item"
            );
            assert!(v >= base, "Delta encoding underflow: v ({}) < base ({})", v, base);
            varint::encode_varint(v - base, self.buffer);
        } else {
            varint::encode_varint(v, self.buffer);
        }
    }

    /// Writes fixed-length raw byte payloads into serialization buffer.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        debug_assert!(!self.done);
        self.buffer.put(bytes);
    }

    /// Resolves and writes the real key byte payload length to the two-byte prefix placeholder.
    pub fn finalize(&mut self) {
        if self.done {
            return;
        }
        let len = self.buffer.as_ref().len() - self.start_pos - 2;
        let len_u16 = u16::try_from(len).expect("Key size exceeds 16-bit unsigned boundary");
        let bytes = len_u16.to_be_bytes();
        self.buffer[self.start_pos] = bytes[0];
        self.buffer[self.start_pos + 1] = bytes[1];
        self.done = true;
    }

    /// Writes an order-preserving varint encoded 64-bit payload to buffer.
    pub fn write_varint(&mut self, v: u64) {
        debug_assert!(!self.done);
        varint::encode_varint(v, self.buffer);
    }

    /// Writes trailing variable-length dynamic strings or vectors to serialization buffer.
    pub fn write_last(&mut self, bytes: &[u8]) {
        debug_assert!(!self.done);
        self.buffer.put(bytes);
        self.finalize();
    }
}

impl<B: Buffer> Drop for KeySerializer<'_, B> {
    fn drop(&mut self) {
        // Validates proper structure usage by requiring explicit finalization invocation.
        if !std::thread::panicking() {
            debug_assert!(self.done);
        }
    }
}

/// Handles decoding of sequential serialized key payloads.
pub struct KeyDeserializer<'a> {
    pub data: &'a [u8],
    base: Option<u64>,
    is_first: bool,
}

impl<'a> KeyDeserializer<'a> {
    /// Parses serialized key with a 2-byte length prefix from the front of payload data.
    pub fn new(data: &'a [u8], base: Option<u64>) -> Result<(Self, usize), Error> {
        ensure!(data.len() >= 2, "Invalid key format: Too short for length header");
        let length = u16::from_be_bytes(data[..2].try_into().unwrap()) as usize;
        ensure!(data.len() >= 2 + length, "Invalid key data payload size");
        Ok((Self { data: &data[2..2 + length], base, is_first: true }, 2 + length))
    }

    /// Constructs deserializer without expecting standard 2-byte size prefix.
    pub fn new_without_prefix(data: &'a [u8]) -> Self {
        Self { data, base: None, is_first: true }
    }

    /// Reads a u64. If a base was provided and this is the first u64, it applies the base.
    pub fn read_u64(&mut self) -> Result<u64, Error> {
        let is_first = self.is_first;
        let v = self.read_varint()?;
        if let Some(base) = self.base.take() {
            assert!(is_first, "read_u64 with base must be the first item");
            Ok(v.checked_add(base).ok_or_else(|| {
                anyhow::anyhow!("Delta decoding overflow: v ({}) + base ({})", v, base)
            })?)
        } else {
            Ok(v)
        }
    }

    /// Extracts an order-preserving decoded 64-bit variable length integer from stream.
    pub fn read_varint(&mut self) -> Result<u64, Error> {
        self.is_first = false;
        let (v, len) = varint::decode_varint(self.data)?;
        self.data = &self.data[len..];
        Ok(v)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], Error> {
        self.is_first = false;
        if self.data.len() < len {
            anyhow::bail!("Data array boundary overrun");
        }
        let (head, tail) = self.data.split_at(len);
        self.data = tail;
        Ok(head)
    }

    fn read_last(&mut self) -> Vec<u8> {
        self.is_first = false;
        let data = self.data;
        self.data = &[];
        data.to_vec()
    }
}

/// Trait defining the translation logic from Fxfs types into order-consistent binaries.
pub trait SerializeKey: Sized {
    /// Encodes key representation sequentially into serialization stream.
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>);

    /// Decodes serializations sequentially from underlying raw bytes.
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error>;
}

impl SerializeKey for u8 {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        serializer.write_bytes(std::slice::from_ref(self));
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        Ok(deserializer.read_bytes(1)?[0])
    }
}

impl SerializeKey for u32 {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        serializer.write_varint(*self as u64)
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        Ok(deserializer.read_varint()?.try_into()?)
    }
}

impl SerializeKey for u64 {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        serializer.write_u64(*self);
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        deserializer.read_u64()
    }
}

impl SerializeKey for String {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        serializer.write_last(self.as_bytes());
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        Ok(String::from_utf8(deserializer.read_last())?)
    }
}

impl SerializeKey for fxfs_unicode::CasefoldString {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        let s: &str = self.as_str();
        serializer.write_last(s.as_bytes());
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        Ok(Self::new(String::deserialize_key_from(deserializer)?))
    }
}

impl SerializeKey for Vec<u8> {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        serializer.write_last(self);
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        Ok(deserializer.read_last())
    }
}

impl SerializeKey for std::ops::Range<u64> {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        // Range upper-bounds are typically critical when evaluating extent allocations
        // in tree merges, so we write end values before start values.
        self.end.serialize_key_to(serializer);
        self.start.serialize_key_to(serializer);
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        let end = u64::deserialize_key_from(deserializer)?;
        let start = u64::deserialize_key_from(deserializer)?;
        Ok(start..end)
    }
}

impl SerializeKey for std::num::NonZeroU64 {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        self.get().serialize_key_to(serializer);
    }
    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, Error> {
        let raw = u64::deserialize_key_from(deserializer)?;
        Self::new(raw).ok_or_else(|| anyhow::anyhow!("Expected non-zero value"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsm_tree::types::OrdUpperBound;
    use crate::object_store::allocator::AllocatorKey;
    use crate::object_store::object_record::{ObjectKey, ObjectKeyData};
    use crate::object_store::{AttributeId, Extent};

    #[test]
    fn test_object_key_order_matches_cmp_upper_bound() {
        let mut keys = Vec::new();
        // Varint edge cases
        keys.push(ObjectKey::object(0));
        keys.push(ObjectKey::object(1));
        keys.push(ObjectKey::object(0xbe));
        keys.push(ObjectKey::object(0xbf));
        keys.push(ObjectKey::object(0xc0));
        keys.push(ObjectKey::object(0x1ffe));
        keys.push(ObjectKey::object(0x1fff));
        keys.push(ObjectKey::object(0x2000));
        keys.push(ObjectKey::object(0x0fff_ffff));
        keys.push(ObjectKey::object(0x1000_0000));

        // String edge cases
        keys.push(ObjectKey { object_id: 1, data: ObjectKeyData::Child { name: "".to_string() } });
        keys.push(ObjectKey { object_id: 1, data: ObjectKeyData::Child { name: "a".to_string() } });
        keys.push(ObjectKey { object_id: 1, data: ObjectKeyData::Child { name: "b".to_string() } });
        keys.push(ObjectKey {
            object_id: 1,
            data: ObjectKeyData::Child { name: "aa".to_string() },
        });
        keys.push(ObjectKey { object_id: 1, data: ObjectKeyData::Child { name: "a".repeat(300) } });

        // Extent edge cases
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 100 * 512..200 * 512));
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 100 * 512..150 * 512));
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 50 * 512..150 * 512));
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 150 * 512..200 * 512));
        keys.push(ObjectKey::extent(2, AttributeId::TEST_ID, 100 * 512..200 * 512));
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 0..100 * 512));
        keys.push(ObjectKey::extent(1, AttributeId::TEST_ID, 50 * 512..100 * 512));

        // Compare all pairs. We compare against `cmp_upper_bound` which is now a total order
        // for ranges (comparing end then start), matching serialization order.
        for i in 0..keys.len() {
            for j in 0..keys.len() {
                let mut buf_a = Vec::new();
                let mut ser_a = KeySerializer::new(&mut buf_a, Some(0));
                keys[i].serialize_key_to(&mut ser_a);
                ser_a.finalize();

                let mut buf_b = Vec::new();
                let mut ser_b = KeySerializer::new(&mut buf_b, Some(0));
                keys[j].serialize_key_to(&mut ser_b);
                ser_b.finalize();

                std::mem::drop(ser_a);
                std::mem::drop(ser_b);

                let cmp = keys[i].cmp_upper_bound(&keys[j]);
                let ser_cmp = compare_keys(&buf_a, &buf_b).unwrap();
                assert_eq!(cmp, ser_cmp, "Mismatch for keys {:?} and {:?}", keys[i], keys[j]);
            }
        }
    }

    #[test]
    fn test_allocator_key_order_matches_cmp_upper_bound() {
        let mut keys = Vec::new();
        keys.push(AllocatorKey { device_range: Extent(0..100 * 512) });
        keys.push(AllocatorKey { device_range: Extent(0..200 * 512) });
        keys.push(AllocatorKey { device_range: Extent(100 * 512..200 * 512) });
        keys.push(AllocatorKey { device_range: Extent(100 * 512..150 * 512) });
        keys.push(AllocatorKey { device_range: Extent(50 * 512..150 * 512) });
        keys.push(AllocatorKey { device_range: Extent(0..50 * 512) });
        keys.push(AllocatorKey { device_range: Extent(50 * 512..100 * 512) });

        // Compare all pairs. We compare against `cmp_upper_bound` which is now a total order
        // for ranges, matching serialization order.
        for i in 0..keys.len() {
            for j in 0..keys.len() {
                let mut buf_a = Vec::new();
                let mut ser_a = KeySerializer::new(&mut buf_a, Some(0));
                keys[i].serialize_key_to(&mut ser_a);
                ser_a.finalize();

                let mut buf_b = Vec::new();
                let mut ser_b = KeySerializer::new(&mut buf_b, Some(0));
                keys[j].serialize_key_to(&mut ser_b);
                ser_b.finalize();

                std::mem::drop(ser_a);
                std::mem::drop(ser_b);

                let cmp = keys[i].cmp_upper_bound(&keys[j]);
                let ser_cmp = compare_keys(&buf_a, &buf_b).unwrap();

                assert_eq!(cmp, ser_cmp, "Mismatch for keys {:?} and {:?}", keys[i], keys[j]);
            }
        }
    }

    #[test]
    fn test_delta_encoding() {
        let mut buf = Vec::new();
        let base = 100;
        let val = 150;

        // Serialize
        {
            let mut ser = KeySerializer::new(&mut buf, Some(base));
            ser.write_u64(val);
            ser.finalize();
        }

        // Deserialize
        let (mut deser, length) = KeyDeserializer::new(&buf, Some(base)).unwrap();
        assert_eq!(length, buf.len());
        let decoded_val = deser.read_u64().unwrap();

        assert_eq!(val, decoded_val);

        // Verify bytes (val - base = 50)
        assert_eq!(buf, vec![0, 1, 50]);
    }

    #[test]
    #[should_panic(expected = "Delta encoding underflow")]
    fn test_delta_encoding_underflow_panics() {
        let mut buf = Vec::new();
        let base = 100;
        let val = 50;

        let mut ser = KeySerializer::new(&mut buf, Some(base));
        ser.write_u64(val);
    }

    #[test]
    fn test_delta_encoding_only_first() {
        let mut buf = Vec::new();
        let base = 100;
        let val1 = 150;
        let val2 = 200;

        // Serialize
        {
            let mut ser = KeySerializer::new(&mut buf, Some(base));
            ser.write_u64(val1);
            ser.write_u64(val2);
            ser.finalize();
        }

        // Deserialize
        let (mut deser, length) = KeyDeserializer::new(&buf, Some(base)).unwrap();
        assert_eq!(length, buf.len());
        let decoded_val1 = deser.read_u64().unwrap();
        let decoded_val2 = deser.read_u64().unwrap();

        assert_eq!(val1, decoded_val1);
        assert_eq!(val2, decoded_val2);
    }

    #[test]
    fn test_delta_encoding_none() {
        let mut buf = Vec::new();
        let val = 150;

        // Serialize
        {
            let mut ser = KeySerializer::new(&mut buf, None);
            ser.write_u64(val);
            ser.finalize();
        }

        // Deserialize
        let (mut deser, length) = KeyDeserializer::new(&buf, None).unwrap();
        assert_eq!(length, buf.len());
        let decoded_val = deser.read_u64().unwrap();

        assert_eq!(val, decoded_val);

        // Verify bytes (should be regular varint of 150, which fits in 1 byte in this encoding)
        assert_eq!(buf, vec![0, 1, 150]);
    }

    #[test]
    fn test_delta_encoding_overflow_on_read_returns_error() {
        // Forge a buffer with a large varint.
        // Varint of u64::MAX is 9 bytes of 0xff.
        let buf = vec![0, 9, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

        // Deserialize with base = Some(1).
        // read_u64 should try to add 1 to u64::MAX and return error.
        let (mut deser, length) = KeyDeserializer::new(&buf, Some(1)).unwrap();
        assert_eq!(length, buf.len());
        assert!(deser.read_u64().is_err());
    }

    #[test]
    #[should_panic(expected = "write_u64 with base must be the first item")]
    fn test_write_u64_with_base_not_first_panics() {
        let mut buf = Vec::new();
        let base = 100;
        let val = 150;

        let mut ser = KeySerializer::new(&mut buf, Some(base));
        ser.write_varint(5); // Write something else first
        ser.write_u64(val); // Should panic here
    }

    #[test]
    #[should_panic(expected = "read_u64 with base must be the first item")]
    fn test_read_u64_with_base_not_first_panics() {
        let mut buf = Vec::new();
        let base = 100;
        let val1 = 150;
        let val2 = 200;

        {
            let mut ser = KeySerializer::new(&mut buf, Some(base));
            ser.write_u64(val1);
            ser.write_u64(val2);
            ser.finalize();
        }

        let (mut deser, length) = KeyDeserializer::new(&buf, Some(base)).unwrap();
        assert_eq!(length, buf.len());
        deser.read_varint().unwrap(); // Reads val1 (delta)
        deser.read_u64().unwrap(); // Tries to read val2 as u64.
    }
}
