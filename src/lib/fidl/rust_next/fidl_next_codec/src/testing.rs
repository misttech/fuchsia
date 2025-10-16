// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{Chunk, Constrained, Decode, Decoded, DecoderExt as _, Encode, EncoderExt as _};

pub fn assert_encoded<T: Encode<Vec<Chunk>>>(value: T, chunks: &[Chunk])
where
    T::Encoded: Constrained<Constraint = ()>,
{
    let mut encoded_chunks = Vec::new();
    encoded_chunks.encode_next(value, ()).unwrap();
    assert_eq!(encoded_chunks, chunks, "encoded chunks did not match");
}

pub fn assert_encoded_with_constraint<T: Encode<Vec<Chunk>>>(
    value: T,
    chunks: &[Chunk],
    constraint: <T::Encoded as Constrained>::Constraint,
) {
    let mut encoded_chunks = Vec::new();
    encoded_chunks.encode_next(value, constraint).unwrap();
    assert_eq!(encoded_chunks, chunks, "encoded chunks did not match");
}

pub fn assert_decoded<T: for<'a> Decode<&'a mut [Chunk]> + Constrained<Constraint = ()>>(
    mut chunks: &mut [Chunk],
    f: impl FnOnce(Decoded<T, &mut [Chunk]>),
) {
    let value = (&mut chunks).decode::<T>().expect("failed to decode");
    f(value)
}

pub fn assert_decoded_with_constraint<T: for<'a> Decode<&'a mut [Chunk]>>(
    mut chunks: &mut [Chunk],
    constraint: T::Constraint,
    f: impl FnOnce(Decoded<T, &mut [Chunk]>),
) {
    let value = (&mut chunks).decode_with_constraint::<T>(constraint).expect("failed to decode");
    f(value)
}
