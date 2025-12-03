// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Debug;
use std::sync::Arc;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

pub type PolicyData = Arc<Vec<u8>>;
pub type PolicyOffset = u32;

#[derive(Clone, Debug, PartialEq)]
pub struct PolicyCursor {
    data: PolicyData,
    offset: PolicyOffset,
}

impl PolicyCursor {
    /// Returns a new [`PolicyCursor`] that wraps `data` in a [`Cursor`] for parsing.
    pub fn new(data: PolicyData) -> Self {
        Self { data, offset: 0 }
    }

    /// Returns a new [`PolicyCursor`] that wraps `data` in a [`Cursor`] for parsing at `offset`.
    pub fn new_at(data: PolicyData, offset: PolicyOffset) -> Self {
        Self { data, offset }
    }

    /// Returns an `P` as the parsed output of the next bytes in the underlying [`Cursor`] data.
    pub fn parse<P: Clone + Debug + FromBytes + KnownLayout + Immutable + PartialEq + Unaligned>(
        mut self,
    ) -> Option<(P, Self)> {
        let (output, _) = P::read_from_prefix(self.remaining_slice()).ok()?;
        self.seek_forward(std::mem::size_of_val(&output)).ok()?;
        Some((output, self))
    }

    pub fn offset(&self) -> PolicyOffset {
        self.offset
    }

    pub fn len(&self) -> usize {
        self.data.len() - self.offset as usize
    }

    /// Seeks forward by `num_bytes`, returning a `std::io::Error` if seeking fails.
    fn seek_forward(&mut self, num_bytes: usize) -> Result<(), std::io::Error> {
        if num_bytes > self.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        self.offset += num_bytes as PolicyOffset;
        Ok(())
    }

    pub fn data(&self) -> &PolicyData {
        &self.data
    }

    /// Returns a slice of remaining data.
    fn remaining_slice(&self) -> &[u8] {
        let s: &[u8] = self.data.as_ref();
        let p = self.offset as usize;
        &s[p..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::little_endian as le;

    #[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
    #[repr(C, packed)]
    struct SomeNumbers {
        a: u8,
        b: le::U32,
        c: le::U16,
        d: u8,
    }

    #[test]
    fn entire_vector() {
        let bytes: Vec<u8> = (0..8).collect();
        let data = Arc::new(bytes);

        let tail = PolicyCursor::new(data);
        let (some_numbers, tail) = tail.parse::<SomeNumbers>().expect("some numbers");

        assert_eq!(0, some_numbers.a);
        assert_eq!((1 << 0) + (2 << 8) + (3 << 16) + (4 << 24), some_numbers.b.get());
        assert_eq!((5 << 0) + (6 << 8), some_numbers.c.get());
        assert_eq!(7, some_numbers.d);
        assert_eq!(8, tail.offset());
        assert_eq!(0, tail.len());
        assert_eq!(8, tail.data().len());
    }

    #[test]
    fn range_within_vector() {
        let bytes: Vec<u8> = (0..40).collect();
        let data = Arc::new(bytes);

        let tail = PolicyCursor::new_at(data, 8);
        let (first_some_numbers, tail) = tail.parse::<SomeNumbers>().expect("some numbers");
        let (second_some_numbers, tail) = tail.parse::<SomeNumbers>().expect("some numbers");
        let (third_some_numbers, tail) = tail.parse::<SomeNumbers>().expect("some numbers");

        assert_eq!(8, first_some_numbers.a);
        assert_eq!((9 << 0) + (10 << 8) + (11 << 16) + (12 << 24), first_some_numbers.b.get());
        assert_eq!((13 << 0) + (14 << 8), first_some_numbers.c.get());
        assert_eq!(15, first_some_numbers.d);
        assert_eq!(16, second_some_numbers.a);
        assert_eq!((17 << 0) + (18 << 8) + (19 << 16) + (20 << 24), second_some_numbers.b.get());
        assert_eq!((21 << 0) + (22 << 8), second_some_numbers.c.get());
        assert_eq!(23, second_some_numbers.d);
        assert_eq!(24, third_some_numbers.a);
        assert_eq!((25 << 0) + (26 << 8) + (27 << 16) + (28 << 24), third_some_numbers.b.get());
        assert_eq!((29 << 0) + (30 << 8), third_some_numbers.c.get());
        assert_eq!(31, third_some_numbers.d);
        assert_eq!(32, tail.offset());
        assert_eq!(8, tail.len());
        assert_eq!(40, tail.data().len());
    }
}
