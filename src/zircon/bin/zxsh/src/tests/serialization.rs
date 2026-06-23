// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::collections::{FlatMap, FlatSet};
use crate::serialization::{Deserialize, Serialize};
use bstr::BString;

fn assert_serialization<T>(val: T, expected_bytes: &[u8])
where
    T: Serialize + Deserialize + PartialEq + std::fmt::Debug,
{
    let mut buf = Vec::new();
    val.serialize_into(&mut buf);
    assert_eq!(buf, expected_bytes, "Serialization mismatch for {:?}", val);

    let mut offset = 0;
    let decoded = T::deserialize(expected_bytes, &mut offset).unwrap();
    assert_eq!(decoded, val, "Deserialization mismatch for {:?}", expected_bytes);
    assert_eq!(offset, expected_bytes.len());
}

#[test]
fn test_u32_serialization() {
    assert_serialization(0u32, &[0, 0, 0, 0]);
    assert_serialization(0x12345678u32, &[0x78, 0x56, 0x34, 0x12]);
    assert_serialization(u32::MAX, &[0xff, 0xff, 0xff, 0xff]);

    let buf = vec![1, 2, 3];
    let mut offset = 0;
    assert!(u32::deserialize(&buf, &mut offset).is_err());
}

#[test]
fn test_i32_serialization() {
    assert_serialization(0i32, &[0, 0, 0, 0]);
    assert_serialization(0x12345678i32, &[0x78, 0x56, 0x34, 0x12]);
    assert_serialization(-1i32, &[0xff, 0xff, 0xff, 0xff]);
    assert_serialization(i32::MAX, &[0xff, 0xff, 0xff, 0x7f]);
    assert_serialization(i32::MIN, &[0, 0, 0, 0x80]);

    let buf = vec![1, 2, 3];
    let mut offset = 0;
    assert!(i32::deserialize(&buf, &mut offset).is_err());
}

#[test]
fn test_u64_serialization() {
    assert_serialization(0u64, &[0, 0, 0, 0, 0, 0, 0, 0]);
    assert_serialization(0x123456789abcdef0u64, &[0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12]);
    assert_serialization(u64::MAX, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);

    let buf = vec![1, 2, 3, 4, 5, 6, 7];
    let mut offset = 0;
    assert!(u64::deserialize(&buf, &mut offset).is_err());
}

#[test]
fn test_bstring_serialization() {
    assert_serialization(BString::from(""), &[0, 0, 0, 0]);
    assert_serialization(BString::from("hello"), &[5, 0, 0, 0, b'h', b'e', b'l', b'l', b'o']);

    // Test EOF
    let mut buf = Vec::new();
    BString::from("hello").serialize_into(&mut buf);
    let mut offset = 0;
    let truncated = &buf[..buf.len() - 1];
    assert!(BString::deserialize(truncated, &mut offset).is_err());
}

#[test]
fn test_vec_u8_serialization() {
    assert_serialization(Vec::<u8>::new(), &[0, 0, 0, 0]);
    assert_serialization(vec![1u8, 2, 3], &[3, 0, 0, 0, 1, 2, 3]);

    let mut buf = Vec::new();
    vec![1, 2, 3].serialize_into(&mut buf);
    let mut offset = 0;
    let truncated = &buf[..buf.len() - 1];
    assert!(Vec::<u8>::deserialize(truncated, &mut offset).is_err());
}

#[test]
fn test_vec_bstring_serialization() {
    assert_serialization(Vec::<BString>::new(), &[0, 0, 0, 0]);
    assert_serialization(
        vec![BString::from("a"), BString::from("b")],
        &[2, 0, 0, 0, 1, 0, 0, 0, b'a', 1, 0, 0, 0, b'b'],
    );

    let mut buf = Vec::new();
    vec![BString::from("hello")].serialize_into(&mut buf);
    let mut offset = 0;
    let truncated = &buf[..buf.len() - 1];
    assert!(Vec::<BString>::deserialize(truncated, &mut offset).is_err());
}

#[test]
fn test_flat_map_serialization() {
    let mut map = FlatMap::new();
    assert_serialization(map.clone(), &[0, 0, 0, 0]);

    map.insert(BString::from("a"), BString::from("b"));
    assert_serialization(map, &[1, 0, 0, 0, 1, 0, 0, 0, b'a', 1, 0, 0, 0, b'b']);
}

#[test]
fn test_flat_set_serialization() {
    let mut set = FlatSet::new();
    assert_serialization(set.clone(), &[0, 0, 0, 0]);

    set.insert(BString::from("a"));
    assert_serialization(set, &[1, 0, 0, 0, 1, 0, 0, 0, b'a']);
}
