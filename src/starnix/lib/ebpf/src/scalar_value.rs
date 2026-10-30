// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use byteorder::{ByteOrder, NativeEndian};
use std::cmp::Ordering;
use zerocopy::IntoBytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range<T: Clone + Copy + std::fmt::Debug + PartialOrd + Ord + PartialEq + Eq> {
    pub min: T,
    pub max: T,
}

impl<T: Clone + Copy + std::fmt::Debug + PartialOrd + Ord + PartialEq + Eq> Range<T> {
    pub const fn new(min: T, max: T) -> Self {
        Self { min, max }
    }
}

impl<T: Clone + Copy + std::fmt::Debug + PartialOrd + Ord + PartialEq + Eq> From<T> for Range<T> {
    fn from(value: T) -> Self {
        Self::new(value, value)
    }
}

macro_rules! make_from {
    ($target_type:ty : $base_from_type:ty {$($types:ty),*}) => {
        $(
            impl From<$types> for $target_type {
                fn from(v: $types) -> Self {
                    (v as $base_from_type).into()
                }
            }
        )*
    }
}

macro_rules! make_scalar_value_data {
    ($t:ident ($($types:ty),*)) => { paste::paste! {

make_from!([< $t:upper Range>]: [< $t >] {$($types),*});
make_from!([< $t:upper ScalarValueData>]: [< $t >] {$($types),*});

pub type [< $t:upper Range>] = Range<[< $t >]>;

impl [< $t:upper Range>] {
    pub const fn max() -> Self {
        Self::new(0, [< $t >]::MAX)
    }

    pub fn extract_slices(value: [< $t >], offset: usize, byte_count: usize) -> ([< $t >], [< $t >], [< $t >]) {
        let v1 = if offset > 0 { NativeEndian::read_uint(&value.as_bytes(), offset) as [< $t >] } else { 0 };
        let v2 = NativeEndian::read_uint(&value.as_bytes()[offset..], byte_count) as [< $t >];
        let v3 = if offset + byte_count < std::mem::size_of::<[< $t >]>() {
            NativeEndian::read_uint(
                &value.as_bytes()[(offset + byte_count)..],
                std::mem::size_of::<[< $t >]>() - offset - byte_count,
            ) as [< $t >]
        } else {
            0
        };
        if cfg!(target_endian = "little") {
            (v1, v2, v3)
        } else {
            (v3, v2, v1)
        }
    }

    pub fn assemble_slices(values: ([< $t >], [< $t >], [< $t >]), offset: usize, byte_count: usize) -> [< $t >] {
        let mut result: [< $t >] = 0;
        let (v1, v2, v3) =
            if cfg!(target_endian = "little") { values } else { (values.2, values.1, values.0) };
        if offset > 0 {
            result.as_mut_bytes()[..offset].copy_from_slice(&v1.as_bytes()[..offset]);
        }
        result.as_mut_bytes()[offset..(offset + byte_count)]
            .copy_from_slice(&v2.as_bytes()[..byte_count]);
        result.as_mut_bytes()[(offset + byte_count)..]
            .copy_from_slice(&v3.as_bytes()[..(std::mem::size_of::<[< $t >]>() - offset - byte_count)]);
        result
    }

    /// Given a target and source values, each in the `target` and `source` ranges. Compute the
    /// range of the result of the operation where `byte_count` bytes from `source` at offset
    /// `source_offset` replace `byte_count` bytes from `target` at offset `target_offset`.
    pub fn compute_range_for_bytes_swap(
        target: [< $t:upper Range>],
        source: [< $t:upper Range>],
        target_offset: usize,
        source_offset: usize,
        byte_count: usize,
    ) -> [< $t:upper Range>] {
        let (target_umin1, target_umin2, target_umin3) =
            Self::extract_slices(target.min, target_offset, byte_count);
        let (target_umax1, target_umax2, target_umax3) =
            Self::extract_slices(target.max, target_offset, byte_count);
        let (_, source_umin2, source_umin3) =
            Self::extract_slices(source.min, source_offset, byte_count);
        let (_, source_umax2, source_umax3) =
            Self::extract_slices(source.max, source_offset, byte_count);

        let (final_umin3, final_umax3) = (target_umin3, target_umax3);
        let (final_umin2, final_umax2) =
            if source_umax3 > source_umin3 { (0, [< $t >]::MAX) } else { (source_umin2, source_umax2) };
        let (final_umin1, final_umax1) =
            if target_umax3 > target_umin3 || target_umax2 > target_umin2 {
                (0, [< $t >]::MAX)
            } else {
                (target_umin1, target_umax1)
            };

        let final_min = Self::assemble_slices(
            (final_umin1, final_umin2, final_umin3),
            target_offset,
            byte_count,
        );
        let final_max = Self::assemble_slices(
            (final_umax1, final_umax2, final_umax3),
            target_offset,
            byte_count,
        );

        [< $t:upper Range>]::new(final_min, final_max)
    }

    pub fn checked_add<T: Into<[< $t:upper Range>]>>(self, rhs: T) -> Option<Self> {
        let rhs = rhs.into();
        let min = self.min.checked_add(rhs.min)?;
        let max = self.max.checked_add(rhs.max)?;
        Some(Self {min, max})
    }

    pub fn checked_sub<T: Into<[< $t:upper Range>]>>(self, rhs: T) -> Option<Self> {
        let rhs = rhs.into();
        let min = self.min.checked_sub(rhs.max)?;
        let max = self.max.checked_sub(rhs.min)?;
        Some(Self {min, max})
    }

    /// Returns whether `[self.min, self.max]` contains at least one integer matching the known
    /// bits `(value, unknown_mask)` (i.e. `(x & !unknown_mask) == value`, assuming
    /// `(value & unknown_mask) == 0`).
    const fn contains_masked_value(self, value: [< $t >], unknown_mask: [< $t >]) -> bool {
        debug_assert!(value & unknown_mask == 0);
        if self.min > self.max {
            return false;
        }
        let diff = self.min ^ self.max;
        if diff == 0 {
            return (self.min ^ value) & !unknown_mask == 0;
        }
        let b = ([< $t >]::BITS - 1) - diff.leading_zeros();
        let common_mask: [< $t >] = if b == [< $t >]::BITS - 1 { 0 } else { !0 << (b + 1) };
        if ((self.min ^ value) & common_mask) & !unknown_mask != 0 {
            return false;
        }
        let value = (value & !common_mask) | (self.min & common_mask);
        let unknown_mask = unknown_mask & !common_mask;
        let bit_b: [< $t >] = 1 << b;
        if unknown_mask & bit_b != 0 {
            (value | (unknown_mask & !bit_b)) >= self.min || (value | bit_b) <= self.max
        } else {
            value <= self.max && self.min <= (value | unknown_mask)
        }
    }
}

impl PartialOrd for [< $t:upper Range>] {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }
        if self.min <= other.min && self.max >= other.max {
            return Some(Ordering::Greater);
        }
        if self.min >= other.min && self.max <= other.max {
            return Some(Ordering::Less);
        }
        None
    }
}

impl<T: Into<[< $t:upper Range>]>> std::ops::Add<T> for [< $t:upper Range>] {
    type Output = Self;
    fn add(self, rhs: T) -> Self {
        let rhs = rhs.into();
        let (min, min_overflowed) = self.min.overflowing_add(rhs.min);
        let (max, max_overflowed) = self.max.overflowing_add(rhs.max);
        if min_overflowed != max_overflowed {
            Self::max()
        } else {
            Self {min, max}
        }
    }
}

impl std::ops::Div for [< $t:upper Range>] {
    type Output = Self;
    fn div(self, rhs: Self) -> Self {
        if rhs.max == 0 {
            return Self::new(0, 0);
        }

        let min = if rhs.min == 0 {
            0
        } else {
            self.min / rhs.max
        };

        let max = self.max / std::cmp::max(1, rhs.min);
        Self { min, max }
    }
}

impl std::ops::Mul for [< $t:upper Range>] {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        let Some(min) = self.min.checked_mul(rhs.min) else {
            return Self::max();
        };
        let Some(max) = self.max.checked_mul(rhs.max) else {
            return Self::max();
        };
        Self { min, max }
    }
}

impl std::ops::Neg for [< $t:upper Range>] {
    type Output = Self;
    fn neg(self) -> Self {
        if self.max == 0 {
            return self
        }
        if self.min == 0 {
            return Self::max()
        }
        Self::new(self.max.wrapping_neg(), self.min.wrapping_neg())
    }
}

impl std::ops::Rem for [< $t:upper Range>] {
    type Output = Self;
    fn rem(self, rhs: Self) -> Self {
        if rhs.max == 0 {
            return self;
        }
        let min = 0;
        let max = if rhs.min == 0 { self.max } else { std::cmp::min(self.max, rhs.max - 1) };
        Self { min, max }
    }
}

impl std::ops::Sub for [< $t:upper Range>] {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        let (min, min_overflowed) = self.min.overflowing_sub(rhs.max);
        let (max, max_overflowed) = self.max.overflowing_sub(rhs.min);
        if min_overflowed != max_overflowed {
            Self::max()
        } else {
            Self {min, max}
        }
    }
}

impl std::ops::Shl for [< $t:upper Range>] {
    type Output = Self;
    fn shl(self, rhs: Self) -> Self {
        // If rhs is not guaranteed to be 32 bits, just return the max range.
        #[allow(irrefutable_let_patterns)]
        let Ok(rhs_max) = u32::try_from(rhs.max) else {
            return Self::max();
        };
        #[allow(irrefutable_let_patterns)]
        let Ok(rhs_min) = u32::try_from(rhs.min) else {
            return Self::max();
        };
        if self.max == 0 {
            return self;
        }
        if rhs_max > self.max.leading_zeros() {
            return Self::max();
        }
        let min = self.min.overflowing_shl(rhs_min).0;
        let max = self.max.overflowing_shl(rhs_max).0;
        Self { min, max }
    }
}

impl std::ops::Shr for [< $t:upper Range>] {
    type Output = Self;
    fn shr(self, rhs: Self) -> Self {
        // ebpf mask shift operation to the number of bytes, if `rhs.max` is greater than the
        // number of bits, no information can be computed, expect that `shr` result is always
        // lesser than the initial value.
        if rhs.max >= $t::BITS.into() {
            return Self { min: 0, max: self.max }
        }
        let rhs_max = rhs.max as u32;
        let rhs_min = rhs.min as u32;
        let min = self.min.overflowing_shr(rhs_max).0;
        let max = self.max.overflowing_shr(rhs_min).0;
        Self { min, max }
    }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct [< $t:upper ScalarValueData>] {
    /// The value. Its interpresentation depends on `unknown_mask` and `unwritten_mask`.
    pub value: [< $t >],
    /// A bit mask of unknown bits. A bit in `value` is valid (and can be used by the verifier)
    /// if the equivalent mask in unknown_mask is 0.
    pub unknown_mask: [< $t >],
    /// A bit mask of unwritten bits. A bit in `value` is written (and can be sent back to
    /// userspace) if the equivalent mask in unwritten_mask is 0. `unwritten_mask` must always be a
    /// subset of `unknown_mask`.
    pub unwritten_mask: [< $t >],
    /// The range of possible unsigned values of this scalar.
    pub urange: [< $t:upper Range >],
    /// Prevent instantiation without using new.
    _guard: (),
}

/// Defines a partial ordering on `ScalarValueData` instances, capturing the notion of how "broad"
/// a scalar value is in terms of the set of potential values it represents.
///
/// The ordering is defined such that `s1 > s2` if a proof that an eBPF program terminates
/// in a state where a register or memory location has a scalar value `s1` is also a proof that
/// the program terminates in a state where that location has scalar value `s2`.
///
/// In other words, a "broader" value represents a larger set of possible values, and
/// proving termination with a broader type implies termination with any narrower value.
///
/// Examples:
/// * `ScalarValueData { unknown_mask: 0, .. }` (a known scalar value) is less than
///   `ScalarValueData { unknown_mask: u64::MAX, .. }` (an unknown scalar value).
impl PartialOrd for [< $t:upper ScalarValueData>] {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        fn mask_is_larger(m1: [< $t >], m2: [< $t >]) -> bool {
            m1 & m2 == m2
        }

        // If the values are equals, return the known result.
        if self == other {
            return Some(Ordering::Equal);
        }

        if mask_is_larger(self.unwritten_mask, other.unwritten_mask)
            && mask_is_larger(self.unknown_mask, other.unknown_mask)
            && self.value & !self.unknown_mask == other.value & !self.unknown_mask
            && self.urange >= other.urange
        {
            return Some(Ordering::Greater);
        }
        if mask_is_larger(other.unwritten_mask, self.unwritten_mask)
            && mask_is_larger(other.unknown_mask, self.unknown_mask)
            && self.value & !other.unknown_mask == other.value & !other.unknown_mask
            && self.urange <= other.urange
        {
            return Some(Ordering::Less);
        }
        None
    }
}

impl From<&[< $t:upper ScalarValueData>]> for [< $t:upper ScalarValueData>] {
    fn from(value: &Self) -> Self {
        value.clone()
    }
}

impl From<[< $t >]> for [< $t:upper ScalarValueData>] {
    fn from(value: [< $t >]) -> Self {
        Self::new(value, 0, 0, value.into())
    }
}

impl [< $t:upper ScalarValueData>] {
    pub const UNINITIALIZED: Self =
        Self::new(0, [< $t >]::MAX, [< $t >]::MAX, [< $t:upper Range >]::max());
    pub const UNKNOWN_WRITTEN: Self =
        Self::new(0, [< $t >]::MAX, 0, [< $t:upper Range >]::max());

    pub const fn new(value: [< $t >], unknown_mask: [< $t >], unwritten_mask: [< $t >], urange: [< $t:upper Range >]) -> Self {
        debug_assert!(value <= urange.max);
        debug_assert!(unknown_mask & unwritten_mask == unwritten_mask);
        debug_assert!(value & unknown_mask == 0);

        let diff = urange.min ^ urange.max;
        let shift = [< $t >]::BITS - diff.leading_zeros();
        let common_mask = if shift == [< $t >]::BITS { 0 } else { !0 << shift };

        let unknown_mask = unknown_mask & !common_mask;
        let unwritten_mask = unwritten_mask & !common_mask;
        let value = (value & !common_mask) | (urange.min & common_mask);

        let urange = if unknown_mask == 0 {
            [< $t:upper Range >]::new(value, value)
        } else {
            let mut min = value;
            if urange.min > min {
                min = urange.min;
            }
            let mut max = value | unknown_mask;
            if urange.max < max {
                max = urange.max;
            }
            [< $t:upper Range >]::new(min, max)
        };
        debug_assert!(unknown_mask & unwritten_mask == unwritten_mask);
        debug_assert!(value & unknown_mask == 0);
        Self { value, unknown_mask, unwritten_mask, urange, _guard: () }
    }

    /// Constructs a `ScalarValueData` if `urange` contains at least one value matching the known
    /// bits `(value, unknown_mask)`, or returns `None` if no value in `urange` matches.
    pub const fn try_new(
        value: [< $t >],
        unknown_mask: [< $t >],
        unwritten_mask: [< $t >],
        urange: [< $t:upper Range >],
    ) -> Option<Self> {
        if urange.contains_masked_value(value, unknown_mask) {
            Some(Self::new(value, unknown_mask, unwritten_mask, urange))
        } else {
            None
        }
    }

    pub const fn is_known(&self) -> bool {
        self.unknown_mask == 0
    }

    pub const fn is_fully_initialized(&self) -> bool {
        self.unwritten_mask == 0
    }

    pub const fn is_uninitialized(&self) -> bool {
        self.unwritten_mask == [< $t >]::MAX
    }

    pub const fn is_zero(&self) -> bool {
        self.is_known() && self.value == 0
    }

    pub const fn min(&self) -> [< $t >] {
        self.urange.min
    }

    pub const fn max(&self) -> [< $t >] {
        self.urange.max
    }

    pub fn update_range(self, urange: [< $t:upper Range >]) -> Self {
        Self::new(self.value, self.unknown_mask, self.unwritten_mask, urange)
    }

    /// Returns whether there exists at least one concrete value belonging to both `self` and
    /// `other`.
    pub fn can_be_equal(self, other: Self) -> bool {
        if (self.value ^ other.value) & !(self.unknown_mask | other.unknown_mask) != 0 {
            return false;
        }
        let range = [< $t:upper Range >]::new(
            std::cmp::max(self.min(), other.min()),
            std::cmp::min(self.max(), other.max()),
        );
        let value = self.value | other.value;
        let unknown_mask = self.unknown_mask & other.unknown_mask;
        range.contains_masked_value(value, unknown_mask)
    }

    pub fn checked_add<T: Into<[< $t:upper ScalarValueData>]>>(self, rhs: T) -> Option<Self> {
        let rhs = rhs.into();
        if self.is_known() && rhs.is_known() {
            return self.value.checked_add(rhs.value).map(Into::into);
        }
        Some(self.base(rhs).update_range(self.urange.checked_add(rhs.urange)?))
    }

    pub fn checked_sub<T: Into<[< $t:upper ScalarValueData>]>>(self, rhs: T) -> Option<Self> {
        let rhs = rhs.into();
        if self.is_known() && rhs.is_known() {
            return self.value.checked_sub(rhs.value).map(Into::into);
        }
        Some(self.base(rhs).update_range(self.urange.checked_sub(rhs.urange)?))
    }

    /// Arithmetic right shift.
    pub fn ashr(self, rhs: Self) -> Self {
        fn ashr(x: [< $t >], y: [< $t >]) -> [< $t >] {
            let x = x.cast_signed();
            // ebpf mask shift operation to the number of bytes, as does `overflowing_sh{rl}`
            // rust operation. So, it is valid to just cast it to `u32`.
            let y = y as u32;
            x.overflowing_shr(y).0.cast_unsigned()
        }
        if !rhs.is_known() {
            return self.base(rhs);
        }
        if self.is_known() {
            return ashr(self.value, rhs.value).into();
        }
        // Arithmetic right shift propagates the sign bit. If the sign bit is unknown,
        // the shifted-in bits must also be marked unknown. We must therefore shift
        // the masks arithmetically to preserve this sign-extended uncertainty.
        // Using logical shift would incorrectly clear the top bits of the masks,
        // causing the verifier to underestimate the maximum possible value.
        let unknown_mask = ashr(self.unknown_mask, rhs.value);
        let unwritten_mask = ashr(self.unwritten_mask, rhs.value);
        let value = ashr(self.value, rhs.value) & !unknown_mask;
        let urange = [< $t:upper Range >]::max();
        Self::new(
            value, unknown_mask, unwritten_mask, urange
        )
    }

    fn shift_operation(self, rhs: [< $t >], urange: [< $t:upper Range >], op: impl Fn([< $t >], [< $t >]) -> [< $t >]) -> Self {
        let value = op(self.value, rhs);
        let unknown_mask = op(self.unknown_mask, rhs);
        let unwritten_mask = op(self.unwritten_mask, rhs);
        Self::new(value, unknown_mask, unwritten_mask, urange)
    }

    fn base(&self, rhs: Self) -> Self {
        if !self.is_fully_initialized() || !rhs.is_fully_initialized() {
            Self::UNINITIALIZED
        } else {
            Self::UNKNOWN_WRITTEN
        }
    }
}

impl<T: Into<[< $t:upper ScalarValueData>]>> std::ops::Add<T> for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn add(self, rhs: T) -> Self {
        let rhs = rhs.into();
        if self.is_known() && rhs.is_known() {
            return self.value.overflowing_add(rhs.value).0.into();
        }
        self.base(rhs).update_range(self.urange + rhs.urange)
    }
}

impl std::ops::Div for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn div(self, rhs: Self) -> Self {
        if self.is_known() && rhs.is_known() {
            if rhs.value  == 0 {
                return 0.into();
            } else {
                return (self.value / rhs.value).into();
            }
        }
        self.base(rhs).update_range(self.urange / rhs.urange)
    }
}

impl std::ops::Mul for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        if self.is_known() && rhs.is_known() {
            return self.value.overflowing_mul(rhs.value).0.into();
        }
        self.base(rhs).update_range(self.urange * rhs.urange)
    }
}

impl std::ops::Neg for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn neg(self) -> Self {
        if self.is_known() {
            return self.value.wrapping_neg().into();
        }
        self.base(self).update_range(-self.urange)
    }
}

impl std::ops::Rem for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn rem(self, rhs: Self) -> Self {
        if self.is_known() && rhs.is_known() {
            if rhs.value == 0 {
                return self;
            } else {
                return (self.value % rhs.value).into();
            }
        }
        self.base(rhs).update_range(self.urange % rhs.urange)
    }
}

impl<T: Into<[< $t:upper ScalarValueData>]>> std::ops::Sub<T> for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn sub(self, rhs: T) -> Self {
        let rhs = rhs.into();
        if self.is_known() && rhs.is_known() {
            return self.value.overflowing_sub(rhs.value).0.into();
        }
        self.base(rhs).update_range(self.urange - rhs.urange)
    }
}

impl std::ops::BitAnd for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        let urange = [< $t:upper Range >]::new(0, std::cmp::min(self.max(), rhs.max()));
        // A bit can be 1 only if it is not known to be 0 in either operand.
        let not_known_zero = (self.value | self.unknown_mask) & (rhs.value | rhs.unknown_mask);
        let unknown_mask = (self.unknown_mask | rhs.unknown_mask) & not_known_zero;
        let unwritten_mask = (self.unwritten_mask | rhs.unwritten_mask) & not_known_zero;
        let value = self.value & rhs.value;
        Self::new(value, unknown_mask, unwritten_mask, urange)
    }
}

impl std::ops::BitOr for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        let urange = [< $t:upper Range >]::new(std::cmp::max(self.min(), rhs.min()), [< $t >]::MAX);
        // Any bit known to be 1 in either operand is known to be 1 in the result.
        let value = self.value | rhs.value;
        let unknown_mask = (self.unknown_mask | rhs.unknown_mask) & !value;
        let unwritten_mask = (self.unwritten_mask | rhs.unwritten_mask) & !value;
        Self::new(value, unknown_mask, unwritten_mask, urange)
    }
}

impl std::ops::BitXor for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        let urange = [< $t:upper Range >]::max();
        let unknown_mask = self.unknown_mask | rhs.unknown_mask;
        let unwritten_mask = self.unwritten_mask | rhs.unwritten_mask;
        let value = (self.value ^ rhs.value) & !unknown_mask;
        Self::new(value, unknown_mask, unwritten_mask, urange)
    }
}

impl std::ops::Shl for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn shl(self, rhs: Self) -> Self {
        let urange = self.urange << rhs.urange;
        if rhs.is_known() {
            return self.shift_operation(rhs.value, urange, |x, y| {
                // ebpf mask shift operation to the number of bytes, as does
                // `overflowing_sh{rl}` rust operation. So, it is valid to just cast it to
                // `u32`.
                let y = y as u32;
                x.overflowing_shl(y).0
            });
        }
        self.base(rhs).update_range(urange)
    }
}

impl std::ops::Shr for [< $t:upper ScalarValueData>] {
    type Output = Self;
    fn shr(self, rhs: Self) -> Self {
        let urange = self.urange >> rhs.urange;
        if rhs.is_known() {
            return self.shift_operation(rhs.value, urange, |x, y| {
                // ebpf mask shift operation to the number of bytes, as does
                // `overflowing_sh{rl}` rust operation. So, it is valid to just cast it to
                // `u32`.
                let y = y as u32;
                x.overflowing_shr(y).0
            });
        }
        self.base(rhs).update_range(urange)
    }
}

}}}
make_scalar_value_data!(u32(i32, u16, i16, u8, i8));
make_scalar_value_data!(u64(i64, u32, i32, u16, i16, u8, i8, usize));
pub type ScalarValueData = U64ScalarValueData;

impl From<U64ScalarValueData> for U32ScalarValueData {
    fn from(v: U64ScalarValueData) -> Self {
        let urange = if v.max() >> 32 == v.min() >> 32 {
            U32Range::new(v.min() as u32, v.max() as u32)
        } else {
            U32Range::max()
        };
        Self::new(v.value as u32, v.unknown_mask as u32, v.unwritten_mask as u32, urange)
    }
}

impl From<U32ScalarValueData> for U64ScalarValueData {
    fn from(v: U32ScalarValueData) -> Self {
        Self::new(
            v.value.into(),
            v.unknown_mask.into(),
            v.unwritten_mask.into(),
            U64Range::new(v.min().into(), v.max().into()),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_checked_sub_interval_arithmetic() {
        // [8,8] - [0,3] = [8-3, 8-0] = [5,8]
        let a = U64Range::new(8, 8);
        let b = U64Range::new(0, 3);
        let result = a.checked_sub(b).unwrap();
        assert_eq!(result.min, 5);
        assert_eq!(result.max, 8);

        // [10,20] - [2,5] = [10-5, 20-2] = [5,18]
        let a = U64Range::new(10, 20);
        let b = U64Range::new(2, 5);
        let result = a.checked_sub(b).unwrap();
        assert_eq!(result.min, 5);
        assert_eq!(result.max, 18);

        // [3,3] - [0,5] = None (3-5 underflows)
        let a = U64Range::new(3, 3);
        let b = U64Range::new(0, 5);
        assert!(a.checked_sub(b).is_none());
    }

    // Regression test: Arithmetic right shift of a fully-unknown u64 must leave the
    // result unbounded. If the sign bit is unknown, all shifted-in bits are also unknown.
    // Before the fix, the masks were logically shifted, which cleared the top bits and
    // incorrectly bounded the range, leading to verifier bypasses.
    #[test]
    fn ashr_fully_unknown_u64_stays_unbounded() {
        let x = U64ScalarValueData::UNKNOWN_WRITTEN;
        assert!(!x.is_known());
        assert_eq!(x.unknown_mask, u64::MAX);
        let shifted = x.ashr(60u64.into());

        assert_eq!(
            shifted.max(),
            u64::MAX,
            "ashr of fully-unknown u64 must leave urange.max = u64::MAX"
        );
        assert_eq!(shifted.unknown_mask, u64::MAX);
    }

    #[test]
    fn ashr_fully_unknown_u32_stays_unbounded() {
        let x = U32ScalarValueData::UNKNOWN_WRITTEN;
        assert!(!x.is_known());
        assert_eq!(x.unknown_mask, u32::MAX);
        let shifted = x.ashr(16u32.into());
        assert_eq!(
            shifted.max(),
            u32::MAX,
            "ashr of fully-unknown u32 must leave urange.max = u32::MAX"
        );
        assert_eq!(shifted.unknown_mask, u32::MAX);
    }

    // Non-negative inputs (known zero sign bit) should still narrow correctly.
    #[test]
    fn ashr_partial_unknown_with_known_zero_msb_narrows_correctly() {
        // Input in [0, 0xFF]. After arsh 4, output should be in [0, 0xF].
        let x = U64ScalarValueData::new(
            /*value=*/ 0,
            /*unknown_mask=*/ 0x00FF,
            /*unwritten_mask=*/ 0,
            U64Range::new(0, 0xFF),
        );
        let shifted = x.ashr(4u64.into());
        assert_eq!(shifted.max(), 0xF);
        assert_eq!(shifted.min(), 0);
        assert_eq!(shifted.unknown_mask, 0xF);
    }

    // Negative inputs (known one sign bit) must sign-extend correctly.
    #[test]
    fn ashr_known_negative_msb_sign_extends() {
        // MSB is 1, bottom byte is unknown.
        let x = U64ScalarValueData::new(
            /*value=*/ 0x8000_0000_0000_0000,
            /*unknown_mask=*/ 0x00FF,
            /*unwritten_mask=*/ 0,
            U64Range::max(),
        );
        let shifted = x.ashr(4u64.into());
        // Result must be sign-extended and retain unknown bottom bits.
        assert_eq!(shifted.value & 0xFFFF_FFFF_FFFF_FF00, 0xF800_0000_0000_0000);
        assert_eq!(shifted.unknown_mask, 0x0F);
    }

    #[test]
    fn test_min_max_known() {
        // Range min == max, should clear unknown mask completely
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(8, 8));
        assert_eq!(data.is_known(), true);
        assert_eq!(data.value, 8);
        assert_eq!(data.unknown_mask, 0);
    }

    #[test]
    fn test_min_max_leading_bits() {
        // Leading common bits: min=4 (0b0100), max=5 (0b0101)
        // Common leading mask: 0b1100
        // bit 2 is 1, bit 1 is 0
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(4, 5));
        assert_eq!(data.unknown_mask & 0b1100, 0);
        assert_eq!(data.value & 0b1100, 0b0100);
        assert_eq!(data.is_known(), false);
    }

    #[test]
    fn test_min_max_no_common_bits() {
        // No common bits: min=0, max=15
        let data = U64ScalarValueData::new(0, u64::MAX, 0, U64Range::new(0, 15));
        assert_eq!(data.unknown_mask, 0xf);
        assert_eq!(data.value, 0);
    }

    #[test]
    fn test_min_max_retains_known_bits() {
        // Initially, bit 3 is known to be 0 (value=0, unknown_mask=0b0111).
        // Range is [4, 5] (0b0100 to 0b0101).
        // Common leading bits for [4, 5] are bits 1..63.
        // Bits 1 and 2 should become known (0 and 1 respectively).
        // Bit 3 should remain known (0).
        let data = U64ScalarValueData::new(0, 0b0111, 0, U64Range::new(4, 5));
        assert_eq!(data.unknown_mask, 0b0001);
        assert_eq!(data.value, 4);
    }

    #[test]
    fn test_unwritten_mask_narrowing() {
        // When urange narrows leading bits (e.g. [0, 1]), those bits become
        // known and written, while the remaining bits stay unknown and
        // unwritten.
        let data = U64ScalarValueData::new(0, u64::MAX, u64::MAX, U64Range::new(0, 1));
        assert!(!data.is_known());
        assert!(!data.is_fully_initialized());
        assert_eq!(data.unknown_mask, 1);
        assert_eq!(data.unwritten_mask, 1);
        assert_eq!(data.min(), 0);
        assert_eq!(data.max(), 1);

        // BitAnd with a non-contiguous mask clears unknown_mask and
        // unwritten_mask on all 0 bits of the mask (even non-leading ones).
        let masked = U64ScalarValueData::UNINITIALIZED & U64ScalarValueData::from(0x0f0fu64);
        assert_eq!(masked.value, 0);
        assert_eq!(masked.unknown_mask, 0x0f0f);
        assert_eq!(masked.unwritten_mask, 0x0f0f);

        // BitOr with a constant clears unknown_mask and unwritten_mask on all 1
        // bits of the constant and sets those bits in value.
        let ored = U64ScalarValueData::UNINITIALIZED | U64ScalarValueData::from(0x0f0fu64);
        assert_eq!(ored.value, 0x0f0f);
        assert_eq!(ored.unknown_mask, !0x0f0f);
        assert_eq!(ored.unwritten_mask, !0x0f0f);

        // Combining BitOr and BitAnd that together overwrite all bits yields a
        // known, initialized constant.
        let initialized = ored & U64ScalarValueData::from(0x0f0fu64);
        assert!(initialized.is_known());
        assert!(initialized.is_fully_initialized());
        assert_eq!(initialized.value, 0x0f0f);
        assert_eq!(initialized.unknown_mask, 0);
        assert_eq!(initialized.unwritten_mask, 0);

        // When urange is a single constant [0, 0] (e.g. `and %r0, 0`), all bits
        // are overwritten with 0, so both unknown_mask and unwritten_mask
        // become 0.
        let zeroed = data & U64ScalarValueData::from(0u64);
        assert!(zeroed.is_known());
        assert!(zeroed.is_fully_initialized());
        assert_eq!(zeroed.value, 0);
        assert_eq!(zeroed.unknown_mask, 0);
        assert_eq!(zeroed.unwritten_mask, 0);

        // Symmetrically, BitOr with all ones (e.g. `or %r0, -1`) overwrites all
        // bits with 1.
        let all_ones = data | U64ScalarValueData::from(u64::MAX);
        assert!(all_ones.is_known());
        assert!(all_ones.is_fully_initialized());
        assert_eq!(all_ones.value, u64::MAX);
        assert_eq!(all_ones.unknown_mask, 0);
        assert_eq!(all_ones.unwritten_mask, 0);

        // BitAnd of an unwritten masked value with an unknown-but-written
        // value: a bit can be 1 only where `masked` is not known to be 0, so
        // the masks shrink to `masked`'s, and the unwritten bits of `masked`
        // stay unwritten.
        let unknown = U64ScalarValueData::UNKNOWN_WRITTEN;
        let and_masked_unknown = masked & unknown;
        assert_eq!(and_masked_unknown.value, 0);
        assert_eq!(and_masked_unknown.unknown_mask, 0x0f0f);
        assert_eq!(and_masked_unknown.unwritten_mask, 0x0f0f);
        assert_eq!(and_masked_unknown.max(), 0x0f0f);

        // BitAnd of a value with known 1 bits (`ored`) with an
        // unknown-but-written value: the known 1 bits become unknown (as the
        // other side is unknown there), but written, as they only depend on the
        // written operand.
        let and_ored_unknown = ored & unknown;
        assert_eq!(and_ored_unknown.value, 0);
        assert_eq!(and_ored_unknown.unknown_mask, u64::MAX);
        assert_eq!(and_ored_unknown.unwritten_mask, !0x0f0f);

        // BitOr of an unwritten masked value with an unknown-but-written value:
        // no bit is known, and only the bits that may come from `masked` remain
        // unwritten.
        let or_masked_unknown = masked | unknown;
        assert_eq!(or_masked_unknown.value, 0);
        assert_eq!(or_masked_unknown.unknown_mask, u64::MAX);
        assert_eq!(or_masked_unknown.unwritten_mask, 0x0f0f);

        // BitOr of `ored` with an unknown-but-written value: the known 1 bits
        // of `ored` are preserved as known and written; everything else is
        // unknown and unwritten.
        let or_ored_unknown = ored | unknown;
        assert_eq!(or_ored_unknown.value, 0x0f0f);
        assert_eq!(or_ored_unknown.unknown_mask, !0x0f0f);
        assert_eq!(or_ored_unknown.unwritten_mask, !0x0f0f);
        assert_eq!(or_ored_unknown.min(), 0x0f0f);

        // Partially known and fully written: only the low byte is unknown.
        let partial = unknown & U64ScalarValueData::from(0xffu64);
        assert_eq!(partial.value, 0);
        assert_eq!(partial.unknown_mask, 0xff);
        assert_eq!(partial.unwritten_mask, 0);

        // BitAnd of two partially unknown values with overlapping masks: only
        // bits that are not known to be 0 on both sides (0xff & 0x0f0f) stay
        // unknown, and among them only the bits unwritten in `masked` stay
        // unwritten.
        let and_partial_masked = partial & masked;
        assert_eq!(and_partial_masked.value, 0);
        assert_eq!(and_partial_masked.unknown_mask, 0x0f);
        assert_eq!(and_partial_masked.unwritten_mask, 0x0f);
        assert_eq!(and_partial_masked.max(), 0x0f);

        // BitOr of the same two values: no bit is known to be 1, so the masks
        // are simply unioned.
        let or_partial_masked = partial | masked;
        assert_eq!(or_partial_masked.value, 0);
        assert_eq!(or_partial_masked.unknown_mask, 0x0fff);
        assert_eq!(or_partial_masked.unwritten_mask, 0x0f0f);

        // Known 1 bits in the low byte, unknown but written elsewhere.
        let known_low = unknown | U64ScalarValueData::from(0xffu64);
        assert_eq!(known_low.value, 0xff);
        assert_eq!(known_low.unknown_mask, !0xff);
        assert_eq!(known_low.unwritten_mask, 0);

        // BitOr with `masked`: the known 1 bits overwrite the unwritten low
        // bits of `masked`, leaving only its high nibble unwritten.
        let or_known_low_masked = known_low | masked;
        assert_eq!(or_known_low_masked.value, 0xff);
        assert_eq!(or_known_low_masked.unknown_mask, !0xff);
        assert_eq!(or_known_low_masked.unwritten_mask, 0x0f00);

        // BitAnd with `ored`: bits known to be 1 on both sides (0xff & 0x0f0f)
        // are known 1, all other bits are unknown, and only the bits unwritten
        // in `ored` are unwritten.
        let and_known_low_ored = known_low & ored;
        assert_eq!(and_known_low_ored.value, 0x0f);
        assert_eq!(and_known_low_ored.unknown_mask, !0x0f);
        assert_eq!(and_known_low_ored.unwritten_mask, !0x0f0f);
        assert_eq!(and_known_low_ored.min(), 0x0f);
    }

    #[test]
    fn test_u32_compute_range_for_bytes_swap() {
        let target = U32Range::new(0x1122_3344, 0x1122_3344);
        let source = U32Range::new(0x0000_aabb, 0x0000_aacc);
        let swapped = U32Range::compute_range_for_bytes_swap(target, source, 1, 0, 2);
        if cfg!(target_endian = "little") {
            assert_eq!(swapped, U32Range::new(0x11aa_bb44, 0x11aa_cc44));
        }
    }

    #[test]
    fn test_contains_masked_value_and_try_new_exhaustive_5bit_and_high_bits() {
        for min in 0u64..32 {
            for max in 0u64..32 {
                let range64 = U64Range::new(min, max);
                let range32 = U32Range::new(min as u32, max as u32);
                for unknown_mask in 0u64..32 {
                    for value in 0u64..32 {
                        if value & unknown_mask != 0 {
                            continue;
                        }
                        let expected =
                            min <= max && (min..=max).any(|x| (x & !unknown_mask) == value);
                        assert_eq!(
                            range64.contains_masked_value(value, unknown_mask),
                            expected,
                            "u64 min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            U64ScalarValueData::try_new(value, unknown_mask, 0, range64).is_some(),
                            expected,
                            "u64 try_new min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            range32.contains_masked_value(value as u32, unknown_mask as u32),
                            expected,
                            "u32 min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                        assert_eq!(
                            U32ScalarValueData::try_new(
                                value as u32,
                                unknown_mask as u32,
                                0,
                                range32,
                            )
                            .is_some(),
                            expected,
                            "u32 try_new min={min} max={max} value={value:#b} unk={unknown_mask:#b}"
                        );
                    }
                }
            }
        }

        // Highest bit (`b == BITS - 1`) coverage for both `u64` and `u32`.
        let top64 = 1u64 << 63;
        let top_range64 = U64Range::new(top64 - 1, top64);
        assert!(top_range64.contains_masked_value(0, top64 | 2));
        assert!(U64ScalarValueData::try_new(0, top64 | 2, 0, top_range64).is_some());
        assert!(!top_range64.contains_masked_value(1, top64 | 2));
        assert!(U64ScalarValueData::try_new(1, top64 | 2, 0, top_range64).is_none());

        let top32 = 1u32 << 31;
        let top_range32 = U32Range::new(top32 - 1, top32);
        assert!(top_range32.contains_masked_value(0, top32 | 2));
        assert!(U32ScalarValueData::try_new(0, top32 | 2, 0, top_range32).is_some());
        assert!(!top_range32.contains_masked_value(1, top32 | 2));
        assert!(U32ScalarValueData::try_new(1, top32 | 2, 0, top_range32).is_none());
    }

    #[test]
    fn test_can_be_equal_common_prefix_and_msb_gap() {
        // Shared `[umin, umax] = [4, 5]` prefix forces bit 2 = 1, turning `val = 2` into `6 > 5`.
        let x = U64ScalarValueData::new(0, 7, 0, U64Range::new(4, 5));
        let y = U64ScalarValueData::new(2, 4, 0, U64Range::new(2, 6));
        assert!(!x.can_be_equal(y));
        assert!(!U32ScalarValueData::from(x).can_be_equal(U32ScalarValueData::from(y)));

        // Gap across the highest differing bit of `umin ^ umax`: `[umin, umax] = [2, 4]` vs
        // combined known bits `(value = 1, unknown_mask = 4)` (`{1, 5}`), where `1 <= 4` and
        // `2 <= 5` both hold yet `{1, 5}` misses `[2, 4]`.
        let a = U64ScalarValueData::new(1, 6, 0, U64Range::new(1, 4));
        let b = U64ScalarValueData::new(0, 5, 0, U64Range::new(2, 5));
        assert!(!a.can_be_equal(b));
        assert!(!U32ScalarValueData::from(a).can_be_equal(U32ScalarValueData::from(b)));
    }
}
