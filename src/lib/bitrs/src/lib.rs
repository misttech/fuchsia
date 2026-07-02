// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(not(test), no_std)]

// So that layout!å works in the test submodule.
#[cfg(test)]
extern crate self as bitrs;

use core::fmt;

/// Specifies a layout of bitfields.
///
/// See the crate's `README.md` for details on syntax and behavior.
pub use bitrs_macro::layout;

//
// The following macros are convenient routines for use in the proc macros.
//
// TODO(https://github.com/rust-lang/rust-project-goals/issues/106): Ideally
// these would be const functions generic over the base type, but that can't be
// properly done until we have const traits.
//

#[doc(hidden)]
#[macro_export]
macro_rules! get_bit {
    ($int:expr, $low_bit:literal) => {
        (($int) & (1 << $low_bit)) != 0
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! set_bit {
    ($int:expr, $low_bit:literal, $value:ident) => {
        if $value {
            $int |= (1 << $low_bit);
        } else {
            $int &= !(1 << $low_bit);
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! shifted_mask {
    ($base:ty, $high_bit:literal, $low_bit:literal) => {{
        const WIDTH: usize = $high_bit - $low_bit + 1;
        if (<$base>::BITS as usize) == WIDTH { <$base>::MAX } else { (1 << WIDTH) - 1 }
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! get_field {
    ($base:ty, $clamped:ty, $high_bit:literal, $low_bit:literal, $shifted:literal, $int:expr) => {{
        const SHIFTED_MASK: $base = $crate::shifted_mask!($base, $high_bit, $low_bit);
        let value = if $shifted {
            ($int >> $low_bit) & SHIFTED_MASK
        } else {
            const UNSHIFTED_MASK: $base = SHIFTED_MASK << $low_bit;
            $int & UNSHIFTED_MASK
        };
        value as $clamped
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! set_field {
    ($base:ty, $high_bit:literal, $low_bit:literal, $shifted:literal, $int:expr, $value:ident) => {
        const SHIFTED_MASK: $base = $crate::shifted_mask!($base, $high_bit, $low_bit);
        const UNSHIFTED_MASK: $base = SHIFTED_MASK << $low_bit;

        $int &= !UNSHIFTED_MASK;
        if $shifted {
            debug_assert!(($value & !SHIFTED_MASK) == 0);
            $int |= ($value & SHIFTED_MASK) << $low_bit;
        } else {
            debug_assert!(($value & !UNSHIFTED_MASK) == 0);
            $int |= $value & UNSHIFTED_MASK;
        }
    };
}

/// Implemented by unsigned integral type, this trait represents a valid base
/// type for a bitfield layout.
pub trait Unsigned: fmt::Debug + private::Sealed {}
impl Unsigned for u8 {}
impl Unsigned for u16 {}
impl Unsigned for u32 {}
impl Unsigned for u64 {}
impl Unsigned for u128 {}

// Ensures that no type outside of bitrs can implement this type.
mod private {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
    impl Sealed for u128 {}
}

/// The metadata of a (non-reserved) bitfield.
///
/// The iterator of a [`layout!`] type will have an associated item type of
/// `(&'static FieldMetadata<Base>, Base)`.
#[derive(Debug)]
pub struct FieldMetadata<Base: Unsigned> {
    /// The name of the bitfield.
    pub name: &'static str,
    /// The high bit of the bitfield.
    pub high_bit: u32,
    /// The low bit of the bitfield.
    pub low_bit: u32,
    /// The default value of the bitfield.
    pub default: Base,
}

#[cfg(test)]
mod tests {
    use super::{FieldMetadata, layout};

    layout!({
        struct EmptyU8(u8);
        {}
    });

    layout!({
        struct OneFieldU16(u16);
        {
            let a @ 15..0;
        }
    });

    layout!({
        struct TwoFieldsU32(u32);
        {
            let a @ 31..16;
            let b @ 15..0;
        }
    });

    layout!({
        struct ThreeFieldsU64(u64);
        {
            let a @ 63..32;
            let b @ 31..16;
            let c @ 15..0;
        }
    });

    layout!({
        struct FourFieldsU128(u128);
        {
            let a @ 127..96;
            let b @ 95..64;
            let c @ 63..32;
            let d @ 31..0;
        }
    });

    #[test]
    fn size_and_alignment() {
        assert_eq!(size_of::<EmptyU8>(), size_of::<u8>());
        assert_eq!(align_of::<EmptyU8>(), align_of::<u8>());

        assert_eq!(size_of::<OneFieldU16>(), size_of::<u16>());
        assert_eq!(align_of::<OneFieldU16>(), align_of::<u16>());

        assert_eq!(size_of::<TwoFieldsU32>(), size_of::<u32>());
        assert_eq!(align_of::<TwoFieldsU32>(), align_of::<u32>());

        assert_eq!(size_of::<ThreeFieldsU64>(), size_of::<u64>());
        assert_eq!(align_of::<ThreeFieldsU64>(), align_of::<u64>());

        assert_eq!(size_of::<FourFieldsU128>(), size_of::<u128>());
        assert_eq!(align_of::<FourFieldsU128>(), align_of::<u128>());
    }

    layout!({
        pub struct Example(u64);
        {
            let u32_repr @ 44..27;
            let __ @ 26..19;
            let __ @ 18..11 = 0xef;
            let with_default @ 10..9 = 0b11;
            let bit @ 8;
            let u8_repr @ 7..4;
            let __ @ 3..2 = 1;
            let __ @ 1..0;
        }
    });

    #[test]
    fn constants() {
        assert_eq!(Example::RSVD1_MASK, (0xef << 11) | (0b01 << 2));
        assert_eq!(Example::RSVD0_MASK, (0x10 << 11) | (0b10 << 2));

        assert_eq!(Example::DEFAULT, (0xef << 11) | (0b01 << 2) | (0b11 << 9));

        assert_eq!(Example::U32_REPR_MASK, 0x1fff_f800_0000);
        assert_eq!(Example::U32_REPR_SHIFT, 27usize,);

        assert_eq!(Example::RSVD_18_11, 0xef << 11);

        assert_eq!(Example::WITH_DEFAULT_MASK, 0x600);
        assert_eq!(Example::WITH_DEFAULT_SHIFT, 9usize);

        assert_eq!(Example::BIT_SHIFT, 8usize);

        assert_eq!(Example::U8_REPR_MASK, 0xf0);
        assert_eq!(Example::U8_REPR_SHIFT, 4usize);

        assert_eq!(Example::RSVD_3_2, 1 << 2);
    }

    // new() should return a value with only reserved-as values set.
    #[test]
    fn new() {
        assert_eq!(Example::new().bits(), Example::RSVD1_MASK);
    }

    // default() should return a value with only defaults and reserved-as
    // values set.
    #[test]
    fn default() {
        assert_eq!(Example::default().bits(), Example::DEFAULT);
    }

    #[test]
    fn from() {
        assert_eq!(Example::from(Example::RSVD1_MASK).bits(), Example::RSVD1_MASK);
        assert_eq!(Example::from(1 | Example::RSVD1_MASK).bits(), 1 | Example::RSVD1_MASK);
        assert_eq!(
            Example::from(0xffff_0000_0000_0000 | Example::RSVD1_MASK).bits(),
            0xffff_0000_0000_0000 | Example::RSVD1_MASK
        );
    }

    #[test]
    fn from_then_get() {
        let example =
            Example::from(0xabcd << 27 | 0b10 << 9 | 1 << 8 | 0xc << 4 | Example::RSVD1_MASK);
        assert_eq!(example.u32_repr(), 0xabcd);
        assert_eq!(example.with_default(), 0b10);
        assert!(example.bit());
        assert_eq!(example.u8_repr(), 0xc);
    }

    #[test]
    fn set_then_get() {
        let example = *Example::new()
            .set_u32_repr(0xabcd)
            .set_with_default(0b10)
            .set_bit(true)
            .set_u8_repr(0xc);
        assert_eq!(example.u32_repr(), 0xabcd);
        assert_eq!(example.with_default(), 0b10);
        assert!(example.bit());
        assert_eq!(example.u8_repr(), 0xc);
    }

    #[test]
    fn iter() {
        type Metadata = FieldMetadata<u64>;

        const EXPECTED: [(u64, Metadata); 4] = [
            (0xabcd, Metadata { name: "u32_repr", high_bit: 44, low_bit: 27, default: 0 }),
            (0b10, Metadata { name: "with_default", high_bit: 10, low_bit: 9, default: 0b11 }),
            (0b1, Metadata { name: "bit", high_bit: 8, low_bit: 8, default: 0 }),
            (0xc, Metadata { name: "u8_repr", high_bit: 7, low_bit: 4, default: 0 }),
        ];

        let example = *Example::new()
            .set_u32_repr(0xabcd)
            .set_with_default(0b10)
            .set_bit(true)
            .set_u8_repr(0xc);

        let actual: Vec<(&'static Metadata, u64)> = example.into_iter().collect();
        let rev_actual: Vec<(&'static Metadata, u64)> = example.into_iter().rev().collect();

        assert_eq!(actual.len(), EXPECTED.len());
        assert_eq!(rev_actual.len(), EXPECTED.len());
        for i in 0..EXPECTED.len() {
            let (expected_val, expected_metadata) = &EXPECTED[i];
            for (label, (actual_metadata, actual_val)) in
                [("fwd", &actual[i]), ("rev", &rev_actual[EXPECTED.len() - 1 - i])]
            {
                assert_eq!(actual_val, expected_val, "{label}:{i}");
                assert_eq!(actual_metadata.name, expected_metadata.name, "{label}:{i}");
                assert_eq!(actual_metadata.high_bit, expected_metadata.high_bit, "{label}:{i}");
                assert_eq!(actual_metadata.low_bit, expected_metadata.low_bit, "{label}:{i}");
                assert_eq!(actual_metadata.default, expected_metadata.default, "{label}:{i}");
            }
        }
    }

    layout!({
        struct Unshifted(u32);
        {
            let field @ 19..16;
            #[unshifted]
            let unshifted_field @ 15..12;
            let __ @ 11..9;
            #[unshifted]
            let unshifted_bit @ 8;
            let normal_bit @ 7;
            let __ @ 6..0;
        }
    });

    #[test]
    fn unshifted_multi_bit_getter() {
        let val = Unshifted::from(0x5 << 12);
        assert_eq!(val.unshifted_field(), 0x5000);
    }

    #[test]
    fn unshifted_multi_bit_setter() {
        let mut val = Unshifted::new();
        val.set_unshifted_field(0xa000);
        assert_eq!(val.unshifted_field(), 0xa000);
        assert_eq!(val.bits() & (0xf << 12), 0xa000);
    }

    #[test]
    fn unshifted_single_bit_getter() {
        let val = Unshifted::from(1 << 8);
        assert_eq!(val.unshifted_bit(), 1 << 8);

        let val = Unshifted::from(0);
        assert_eq!(val.unshifted_bit(), 0);
    }

    #[test]
    fn unshifted_single_bit_setter() {
        let mut val = Unshifted::new();
        val.set_unshifted_bit(1 << 8);
        assert_eq!(val.unshifted_bit(), 1 << 8);

        val.set_unshifted_bit(0);
        assert_eq!(val.unshifted_bit(), 0);
    }

    #[test]
    fn unshifted_round_trip() {
        let val = *Unshifted::new()
            .set_unshifted_field(0x7000)
            .set_unshifted_bit(1 << 8)
            .set_field(0xa)
            .set_normal_bit(true);
        assert_eq!(val.unshifted_field(), 0x7000);
        assert_eq!(val.unshifted_bit(), 1 << 8);
        assert_eq!(val.field(), 0xa);
        assert!(val.normal_bit());
    }

    #[test]
    fn unshifted_ignores_other_bits() {
        let val = Unshifted::from(0xffff_ffff);
        assert_eq!(val.unshifted_field(), 0xf000);
        assert_eq!(val.unshifted_bit(), 1 << 8);
    }
}
