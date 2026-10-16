// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::expression::{DivExpression, MulExpression};
use crate::fixed::{Fixed, PromoteAddSub, PromoteMul};
use crate::num_traits::{
    self, Bounded, CheckedDiv, SaturatingAdd, SaturatingMul, SaturatingSub, Zero,
};
use crate::utility::{clamp_cast, round_half_to_even};

mod private {
    pub trait Sealed {}
    impl Sealed for i8 {}
    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

pub trait FastInt:
    private::Sealed + Copy + num_traits::NumCast + num_traits::Bounded + PartialOrd + num_traits::Zero
{
    type Intermediate: num_traits::PrimInt
        + num_traits::Bounded
        + num_traits::SaturatingAdd
        + num_traits::SaturatingSub
        + num_traits::SaturatingMul
        + num_traits::CheckedDiv
        + num_traits::NumCast;
}

impl FastInt for i8 {
    type Intermediate = i64;
}
impl FastInt for i16 {
    type Intermediate = i64;
}
impl FastInt for i32 {
    type Intermediate = i64;
}
impl FastInt for i64 {
    type Intermediate = i64;
}

impl FastInt for u8 {
    type Intermediate = u64;
}
impl FastInt for u16 {
    type Intermediate = u64;
}
impl FastInt for u32 {
    type Intermediate = u64;
}
impl FastInt for u64 {
    type Intermediate = u64;
}

#[inline]
pub(crate) fn saturating_shl<I>(val: I, bits: usize) -> I
where
    I: num_traits::PrimInt + num_traits::Bounded + num_traits::SaturatingMul,
{
    if bits == 0 || val == I::zero() {
        return val;
    }
    let integer_bits = core::mem::size_of::<I>() * 8;
    let limit = if I::min_value() < I::zero() { integer_bits - 1 } else { integer_bits };
    if bits >= limit {
        if I::min_value() < I::zero() {
            // Signed
            return if val > I::zero() { I::max_value() } else { I::min_value() };
        } else {
            // Unsigned
            return I::max_value();
        }
    }
    let shift_factor = I::one() << bits;
    val.saturating_mul(&shift_factor)
}

#[inline]
pub(crate) fn div_power_of_two<I>(val: I, bits: usize) -> I
where
    I: num_traits::PrimInt + num_traits::Bounded,
{
    let integer_bits = core::mem::size_of::<I>() * 8;
    let limit = if I::min_value() < I::zero() { integer_bits - 1 } else { integer_bits };
    if bits == integer_bits - 1 && val == I::min_value() && I::min_value() < I::zero() {
        I::zero().saturating_sub(&I::one())
    } else if bits >= limit {
        I::zero()
    } else {
        val / (I::one() << bits)
    }
}

/// A wrapper around Fixed that forces intermediate calculations to be performed
/// in 64-bit space for maximum execution efficiency on 64-bit targets.
///
/// # Warning: Range Reduction at High Resolutions
///
/// Because intermediate calculations (such as multiplication and division) are performed
/// within 64-bit integers (either signed `i64` or unsigned `u64` depending on the operands)
/// before rescaling, `FastFixed` saturates significantly earlier than 128-bit `Fixed`.
///
/// For high-resolution formats (e.g., `FRAC >= 32`), even multiplying modest values
/// like `1.0 * 1.0` can result in early saturation (or scaling down to smaller values)
/// because the 64-bit intermediate product overflows. Use `FastFixed` primarily when
/// fractional bit resolutions are small enough to accommodate your expected dynamic range
/// within 64 bits.
#[repr(transparent)]
#[derive(Clone, Copy, Default, Hash)]
#[allow(clippy::derived_hash_with_manual_eq)]
pub struct FastFixed<I, const FRAC: usize>(pub Fixed<I, FRAC>);

impl<I: PartialEq + Copy, const FRAC: usize> PartialEq for FastFixed<I, FRAC> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.raw_value() == other.0.raw_value()
    }
}

impl<I: Eq + Copy, const FRAC: usize> Eq for FastFixed<I, FRAC> {}

impl<I: PartialOrd + Copy, const FRAC: usize> PartialOrd for FastFixed<I, FRAC> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.raw_value().partial_cmp(&other.0.raw_value())
    }
}

impl<I: Ord + Copy, const FRAC: usize> Ord for FastFixed<I, FRAC> {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.raw_value().cmp(&other.0.raw_value())
    }
}

impl<I, const FRAC: usize> core::fmt::Debug for FastFixed<I, FRAC>
where
    I: Into<i128> + Copy + core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = crate::string::String::new(self.0, crate::string::Mode::Dec, None);
        write!(f, "FastFixed(raw: {:?}, value: {})", self.0.raw_value(), s)
    }
}

impl<I: Copy, const FRAC: usize> FastFixed<I, FRAC> {
    /// Creates a FastFixed value from a raw underlying integer value.
    pub const fn from_raw(value: I) -> Self {
        Self(Fixed::from_raw(value))
    }

    /// Returns the raw fixed-point value as the underlying integer type.
    pub const fn raw_value(&self) -> I {
        self.0.raw_value()
    }

    /// Returns the underlying Fixed value.
    pub const fn into_fixed(self) -> Fixed<I, FRAC> {
        self.0
    }
}

impl<I: Bounded + num_traits::NumCast + Copy, const FRAC: usize> FastFixed<I, FRAC> {
    /// Returns the minimum value of this fixed point format.
    pub fn min() -> Self {
        Self(Fixed::min())
    }

    /// Returns the maximum value of this fixed point format.
    pub fn max() -> Self {
        Self(Fixed::max())
    }
}

impl<I: Copy, const FRAC: usize> From<Fixed<I, FRAC>> for FastFixed<I, FRAC> {
    fn from(val: Fixed<I, FRAC>) -> Self {
        Self(val)
    }
}

impl<I: Copy, const FRAC: usize> From<FastFixed<I, FRAC>> for Fixed<I, FRAC> {
    fn from(val: FastFixed<I, FRAC>) -> Self {
        val.0
    }
}

// Negation
impl<I, const FRAC: usize> core::ops::Neg for FastFixed<I, FRAC>
where
    I: num_traits::Signed
        + num_traits::Bounded
        + num_traits::Zero
        + PartialOrd
        + num_traits::NumCast
        + Copy
        + FastInt,
{
    type Output = Self;

    fn neg(self) -> Self::Output {
        type Intermediate<I> = <I as FastInt>::Intermediate;
        let val_64: Intermediate<I> = clamp_cast(self.0.raw_value());
        let negated = Intermediate::<I>::zero().saturating_sub(&val_64);
        Self(Fixed::from_raw(clamp_cast(negated)))
    }
}

// Operators: Add
impl<T, const FRAC: usize, U, const FRAC_R: usize> core::ops::Add<FastFixed<U, FRAC_R>>
    for FastFixed<T, FRAC>
where
    T: PromoteAddSub<U> + num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    U: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    <T as PromoteAddSub<U>>::Output: FastInt,
{
    type Output = FastFixed<<T as PromoteAddSub<U>>::Output, FRAC>;

    fn add(self, rhs: FastFixed<U, FRAC_R>) -> Self::Output {
        type Out<T, U> = <T as PromoteAddSub<U>>::Output;
        type I64<T, U> = <Out<T, U> as FastInt>::Intermediate;

        let val_l: I64<T, U> = clamp_cast(self.0.raw_value());
        let val_r: I64<T, U> = clamp_cast(rhs.0.raw_value());

        // Perform alignment shift in 64-bit space
        let sum = if FRAC > FRAC_R {
            let delta = FRAC - FRAC_R;
            let val_r_shifted = saturating_shl(val_r, delta);
            val_l.saturating_add(&val_r_shifted)
        } else if FRAC < FRAC_R {
            let delta = FRAC_R - FRAC;
            let val_l_shifted = saturating_shl(val_l, delta);
            let sum_aligned = val_l_shifted.saturating_add(&val_r);
            let rounded = round_half_to_even::<I64<T, U>>(sum_aligned, delta);
            div_power_of_two(rounded, delta)
        } else {
            val_l.saturating_add(&val_r)
        };

        FastFixed(Fixed::from_raw(clamp_cast(sum)))
    }
}

// Operators: Sub
impl<T, const FRAC: usize, U, const FRAC_R: usize> core::ops::Sub<FastFixed<U, FRAC_R>>
    for FastFixed<T, FRAC>
where
    T: PromoteAddSub<U> + num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    U: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    <T as PromoteAddSub<U>>::Output: FastInt,
{
    type Output = FastFixed<<T as PromoteAddSub<U>>::Output, FRAC>;

    fn sub(self, rhs: FastFixed<U, FRAC_R>) -> Self::Output {
        type Out<T, U> = <T as PromoteAddSub<U>>::Output;
        type I64<T, U> = <Out<T, U> as FastInt>::Intermediate;

        let val_l: I64<T, U> = clamp_cast(self.0.raw_value());
        let val_r: I64<T, U> = clamp_cast(rhs.0.raw_value());

        // Perform alignment shift in 64-bit space
        let diff = if FRAC > FRAC_R {
            let delta = FRAC - FRAC_R;
            let val_r_shifted = saturating_shl(val_r, delta);
            val_l.saturating_sub(&val_r_shifted)
        } else if FRAC < FRAC_R {
            let delta = FRAC_R - FRAC;
            let val_l_shifted = saturating_shl(val_l, delta);
            let diff_aligned = val_l_shifted.saturating_sub(&val_r);
            let rounded = round_half_to_even::<I64<T, U>>(diff_aligned, delta);
            div_power_of_two(rounded, delta)
        } else {
            val_l.saturating_sub(&val_r)
        };

        FastFixed(Fixed::from_raw(clamp_cast(diff)))
    }
}

// Operators: Mul (returns a deferred MulExpression containing FastFixed)
impl<T, const FL: usize, U, const FR: usize> core::ops::Mul<FastFixed<U, FR>> for FastFixed<T, FL> {
    type Output = MulExpression<FastFixed<T, FL>, FastFixed<U, FR>>;

    fn mul(self, rhs: FastFixed<U, FR>) -> Self::Output {
        MulExpression { lhs: self, rhs }
    }
}

/// A deferred `FastFixed` expression.
///
/// Expressions carry the exact result of an operation (within the 64-bit
/// intermediate type) together with its number of fractional bits, and are
/// rounded exactly once when they are converted into a concrete `FastFixed`
/// format with `From`/`Into` or [`FastExpression::evaluate`], mirroring the
/// deferred expression templates of the C++ library. Expressions compose:
/// adding or subtracting two expressions yields a deferred [`SumExpression`],
/// while combining or comparing an expression with a `FastFixed` evaluates
/// the expression in that `FastFixed`'s format.
pub trait FastExpression: Copy {
    /// The 64-bit intermediate integer type the expression is evaluated in.
    type Int: num_traits::PrimInt
        + Bounded
        + SaturatingAdd
        + SaturatingSub
        + SaturatingMul
        + num_traits::NumCast
        + crate::num_traits::FixedInt;

    /// The number of fractional bits of the raw value produced by `evaluate_raw`.
    const FRAC: usize;

    /// Evaluates the expression to its raw value with `Self::FRAC` fractional bits.
    fn evaluate_raw(self) -> Self::Int;

    /// Evaluates the expression into the given `FastFixed` format, rounding half
    /// to even exactly once and saturating when the value does not fit.
    fn evaluate<I, const FRAC_OUT: usize>(self) -> FastFixed<I, FRAC_OUT>
    where
        I: Bounded + num_traits::NumCast + Copy,
    {
        FastFixed(Fixed::from_raw(clamp_cast(rescale(self.evaluate_raw(), Self::FRAC, FRAC_OUT))))
    }
}

/// Rescales a raw value from `src_frac` to `dst_frac` fractional bits, rounding
/// half to even when precision is reduced and saturating when it is increased.
#[inline]
fn rescale<I>(value: I, src_frac: usize, dst_frac: usize) -> I
where
    I: num_traits::PrimInt + Bounded + SaturatingMul + crate::num_traits::FixedInt,
{
    if src_frac > dst_frac {
        let delta = src_frac - dst_frac;
        div_power_of_two(round_half_to_even::<I>(value, delta), delta)
    } else if src_frac < dst_frac {
        saturating_shl(value, dst_frac - src_frac)
    } else {
        value
    }
}

impl<T, const FL: usize, U, const FR: usize> FastExpression
    for MulExpression<FastFixed<T, FL>, FastFixed<U, FR>>
where
    T: PromoteMul<U> + num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    U: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    <T as PromoteMul<U>>::Output: FastInt,
{
    type Int = <<T as PromoteMul<U>>::Output as FastInt>::Intermediate;
    const FRAC: usize = FL + FR;

    fn evaluate_raw(self) -> Self::Int {
        let val_l: Self::Int = clamp_cast(self.lhs.0.raw_value());
        let val_r: Self::Int = clamp_cast(self.rhs.0.raw_value());
        val_l.saturating_mul(&val_r)
    }
}

/// A deferred sum or difference of two `FastFixed` expressions.
///
/// The operands are aligned to the lesser of their precisions before the
/// addition, as in the C++ library.
#[derive(Clone, Copy)]
pub struct SumExpression<L, R> {
    lhs: L,
    rhs: R,
    subtract: bool,
}

impl<L, R> FastExpression for SumExpression<L, R>
where
    L: FastExpression,
    R: FastExpression<Int = L::Int>,
{
    type Int = L::Int;
    const FRAC: usize = if L::FRAC < R::FRAC { L::FRAC } else { R::FRAC };

    fn evaluate_raw(self) -> Self::Int {
        let lhs = rescale(self.lhs.evaluate_raw(), L::FRAC, Self::FRAC);
        let rhs = rescale(self.rhs.evaluate_raw(), R::FRAC, Self::FRAC);
        if self.subtract { lhs.saturating_sub(&rhs) } else { lhs.saturating_add(&rhs) }
    }
}

// Evaluation of expressions into a FastFixed format.
impl<I, const FRAC_OUT: usize, L, R> From<MulExpression<L, R>> for FastFixed<I, FRAC_OUT>
where
    I: Bounded + num_traits::NumCast + Copy,
    MulExpression<L, R>: FastExpression,
{
    fn from(expr: MulExpression<L, R>) -> Self {
        expr.evaluate()
    }
}

impl<I, const FRAC_OUT: usize, L, R> From<SumExpression<L, R>> for FastFixed<I, FRAC_OUT>
where
    I: Bounded + num_traits::NumCast + Copy,
    SumExpression<L, R>: FastExpression,
{
    fn from(expr: SumExpression<L, R>) -> Self {
        expr.evaluate()
    }
}

// Comparison of a FastFixed with an expression: the expression is evaluated in
// the format of the FastFixed operand, as in the C++ library.
impl<I, const FRAC: usize, E> PartialEq<E> for FastFixed<I, FRAC>
where
    I: Bounded + num_traits::NumCast + PartialEq + Copy,
    E: FastExpression,
{
    fn eq(&self, other: &E) -> bool {
        *self == other.evaluate::<I, FRAC>()
    }
}

impl<I, const FRAC: usize, E> PartialOrd<E> for FastFixed<I, FRAC>
where
    I: Bounded + num_traits::NumCast + PartialOrd + Copy,
    E: FastExpression,
{
    fn partial_cmp(&self, other: &E) -> Option<core::cmp::Ordering> {
        self.partial_cmp(&other.evaluate::<I, FRAC>())
    }
}

/// Addition and subtraction involving expressions. A FastFixed operand fixes the
/// result format: the expression is evaluated in that format first, which matches
/// the C++ promotion to the lesser precision whenever the FastFixed operand is at
/// most as precise as the expression. Two expressions combine into a deferred
/// SumExpression.
macro_rules! impl_fast_fixed_expression_add_sub {
    ($Trait:ident, $method:ident, $op:tt) => {
        impl<I, const FRAC: usize, E> core::ops::$Trait<E> for FastFixed<I, FRAC>
        where
            I: Bounded + num_traits::NumCast + Copy,
            E: FastExpression,
            FastFixed<I, FRAC>: core::ops::$Trait<FastFixed<I, FRAC>>,
        {
            type Output = <FastFixed<I, FRAC> as core::ops::$Trait<FastFixed<I, FRAC>>>::Output;

            fn $method(self, rhs: E) -> Self::Output {
                self $op rhs.evaluate::<I, FRAC>()
            }
        }
    };
}

impl_fast_fixed_expression_add_sub!(Add, add, +);
impl_fast_fixed_expression_add_sub!(Sub, sub, -);

/// Expression-side operators. These are implemented per expression type rather
/// than as a blanket impl over `FastExpression` because the orphan rules forbid
/// implementing a foreign trait for a bare type parameter.
macro_rules! impl_fast_expression_ops {
    ($Expr:ident) => {
        impl<I, const FRAC: usize, L, R> PartialEq<FastFixed<I, FRAC>> for $Expr<L, R>
        where
            I: Bounded + num_traits::NumCast + PartialEq + Copy,
            $Expr<L, R>: FastExpression,
        {
            fn eq(&self, other: &FastFixed<I, FRAC>) -> bool {
                self.evaluate::<I, FRAC>() == *other
            }
        }

        impl<I, const FRAC: usize, L, R> PartialOrd<FastFixed<I, FRAC>> for $Expr<L, R>
        where
            I: Bounded + num_traits::NumCast + PartialOrd + Copy,
            $Expr<L, R>: FastExpression,
        {
            fn partial_cmp(&self, other: &FastFixed<I, FRAC>) -> Option<core::cmp::Ordering> {
                self.evaluate::<I, FRAC>().partial_cmp(other)
            }
        }

        impl_fast_expression_ops!(@add_sub $Expr, Add, add, +, false);
        impl_fast_expression_ops!(@add_sub $Expr, Sub, sub, -, true);
    };
    (@add_sub $Expr:ident, $Trait:ident, $method:ident, $op:tt, $subtract:expr) => {
        impl<I, const FRAC: usize, L, R> core::ops::$Trait<FastFixed<I, FRAC>> for $Expr<L, R>
        where
            I: Bounded + num_traits::NumCast + Copy,
            $Expr<L, R>: FastExpression,
            FastFixed<I, FRAC>: core::ops::$Trait<FastFixed<I, FRAC>>,
        {
            type Output = <FastFixed<I, FRAC> as core::ops::$Trait<FastFixed<I, FRAC>>>::Output;

            fn $method(self, rhs: FastFixed<I, FRAC>) -> Self::Output {
                self.evaluate::<I, FRAC>() $op rhs
            }
        }

        impl<L, R, E> core::ops::$Trait<E> for $Expr<L, R>
        where
            $Expr<L, R>: FastExpression,
            E: FastExpression,
        {
            type Output = SumExpression<$Expr<L, R>, E>;

            fn $method(self, rhs: E) -> Self::Output {
                SumExpression { lhs: self, rhs, subtract: $subtract }
            }
        }
    };
}

impl_fast_expression_ops!(MulExpression);
impl_fast_expression_ops!(SumExpression);

// Format conversion.
impl<R, const FRAC_OUT: usize> FastFixed<R, FRAC_OUT>
where
    R: Bounded + num_traits::NumCast + Copy,
{
    /// Converts `other` into this format, rounding half to even when precision
    /// is reduced and saturating when the value does not fit.
    ///
    /// This is the equivalent of the C++ `ffl` idiom `Target{other}` and replaces
    /// the `Fixed::from_fixed(other.into_fixed()).into()` round trip.
    pub fn from_fixed<OtherI: Copy, const OTHER_FRAC: usize>(
        other: FastFixed<OtherI, OTHER_FRAC>,
    ) -> Self
    where
        crate::fixed_format::Value<OtherI, OTHER_FRAC>:
            crate::fixed_format::ConvertValue<R, FRAC_OUT>,
    {
        FastFixed(Fixed::from_fixed(other.0))
    }
}

// Operators: Div (returns a DivExpression containing FastFixed)
impl<IL, const FL: usize, IR, const FR: usize> core::ops::Div<FastFixed<IR, FR>>
    for FastFixed<IL, FL>
{
    type Output = DivExpression<FastFixed<IL, FL>, FastFixed<IR, FR>>;

    fn div(self, rhs: FastFixed<IR, FR>) -> Self::Output {
        DivExpression { numerator: self, denominator: rhs }
    }
}

impl<IL, const FL: usize, IR, const FR: usize> DivExpression<FastFixed<IL, FL>, FastFixed<IR, FR>> {
    /// Evaluates the deferred division into a FastFixed value of the target format.
    ///
    /// # Panics
    ///
    /// Panics if the denominator is zero.
    pub fn evaluate<I, const TARGET_FRAC: usize>(self) -> FastFixed<I, TARGET_FRAC>
    where
        FastFixed<I, TARGET_FRAC>: From<Self>,
    {
        FastFixed::from(self)
    }
}

// Division assignment evaluation
impl<I, const TARGET_FRAC: usize, IL, const FL: usize, IR, const FR: usize>
    From<DivExpression<FastFixed<IL, FL>, FastFixed<IR, FR>>> for FastFixed<I, TARGET_FRAC>
where
    I: num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
    IL: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    IR: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
{
    fn from(expr: DivExpression<FastFixed<IL, FL>, FastFixed<IR, FR>>) -> Self {
        type I64<I> = <I as FastInt>::Intermediate;
        let num_raw: I64<I> = clamp_cast(expr.numerator.0.raw_value());
        let denom_raw: I64<I> = clamp_cast(expr.denominator.0.raw_value());

        let dst_frac = TARGET_FRAC + FR;

        let num_scaled = if FL > dst_frac {
            let delta = FL - dst_frac;
            let rounded = round_half_to_even::<I64<I>>(num_raw, delta);
            div_power_of_two(rounded, delta)
        } else if FL < dst_frac {
            let delta = dst_frac - FL;
            saturating_shl(num_raw, delta)
        } else {
            num_raw
        };

        if denom_raw == I64::<I>::zero() {
            panic!("Division by zero in DivExpression");
        }
        let quotient = if let Some(q) = num_scaled.checked_div(&denom_raw) {
            q
        } else {
            I64::<I>::max_value()
        };
        FastFixed(Fixed::from_raw(clamp_cast(quotient)))
    }
}

// Compound assignments
impl<I, const FRAC: usize, U, const FRAC_R: usize> core::ops::AddAssign<FastFixed<U, FRAC_R>>
    for FastFixed<I, FRAC>
where
    I: PromoteAddSub<U>
        + num_traits::Zero
        + PartialOrd
        + num_traits::Bounded
        + num_traits::NumCast
        + Copy
        + FastInt
        + TryFrom<<I as PromoteAddSub<U>>::Output>,
    <I as PromoteAddSub<U>>::Output: FastInt,
    U: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    FastFixed<I, FRAC>: core::ops::Add<
            FastFixed<U, FRAC_R>,
            Output = FastFixed<<I as PromoteAddSub<U>>::Output, FRAC>,
        >,
{
    fn add_assign(&mut self, rhs: FastFixed<U, FRAC_R>) {
        let sum = *self + rhs;
        self.0 = Fixed::from_raw(clamp_cast(sum.0.raw_value()));
    }
}

impl<I, const FRAC: usize, U, const FRAC_R: usize> core::ops::SubAssign<FastFixed<U, FRAC_R>>
    for FastFixed<I, FRAC>
where
    I: PromoteAddSub<U>
        + num_traits::Zero
        + PartialOrd
        + num_traits::Bounded
        + num_traits::NumCast
        + Copy
        + FastInt
        + TryFrom<<I as PromoteAddSub<U>>::Output>,
    <I as PromoteAddSub<U>>::Output: FastInt,
    U: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
    FastFixed<I, FRAC>: core::ops::Sub<
            FastFixed<U, FRAC_R>,
            Output = FastFixed<<I as PromoteAddSub<U>>::Output, FRAC>,
        >,
{
    fn sub_assign(&mut self, rhs: FastFixed<U, FRAC_R>) {
        let diff = *self - rhs;
        self.0 = Fixed::from_raw(clamp_cast(diff.0.raw_value()));
    }
}

impl<I, const FRAC: usize> core::ops::MulAssign<FastFixed<I, FRAC>> for FastFixed<I, FRAC>
where
    I: PromoteMul<I>
        + num_traits::Zero
        + PartialOrd
        + num_traits::Bounded
        + num_traits::NumCast
        + Copy
        + FastInt
        + TryFrom<<I as PromoteMul<I>>::Output>,
    <I as PromoteMul<I>>::Output: FastInt,
    FastFixed<I, FRAC>:
        core::ops::Mul<FastFixed<I, FRAC>, Output = FastFixed<<I as PromoteMul<I>>::Output, FRAC>>,
{
    fn mul_assign(&mut self, rhs: Self) {
        let prod = *self * rhs;
        self.0 = Fixed::from_raw(clamp_cast(prod.0.raw_value()));
    }
}

impl<I, const FRAC: usize> core::ops::DivAssign<FastFixed<I, FRAC>> for FastFixed<I, FRAC>
where
    Self: Copy,
    I: num_traits::Zero + PartialOrd,
    FastFixed<I, FRAC>: From<DivExpression<Self, Self>>,
{
    fn div_assign(&mut self, rhs: Self) {
        *self = Self::from(DivExpression { numerator: *self, denominator: rhs });
    }
}

// Display, LowerHex implementations
impl<I, const FRAC: usize> core::fmt::Display for FastFixed<I, FRAC>
where
    I: Into<i128> + Copy,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = crate::string::String::new(self.0, crate::string::Mode::Dec, f.precision());
        write!(f, "{}", s)
    }
}

impl<I, const FRAC: usize> core::fmt::LowerHex for FastFixed<I, FRAC>
where
    I: Into<i128> + Copy,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = crate::string::String::new(self.0, crate::string::Mode::Hex, Some(0));
        write!(f, "{}", s)
    }
}

// Commutative relational operators between FastFixed and raw integers
macro_rules! impl_fast_fixed_integer_comparison {
    ($T:ty, $Int:ty) => {
        impl<const FRAC: usize> PartialEq<$Int> for FastFixed<$T, FRAC>
        where
            Fixed<$T, FRAC>: PartialEq<$Int>,
        {
            fn eq(&self, other: &$Int) -> bool {
                self.0 == *other
            }
        }

        impl<const FRAC: usize> PartialEq<FastFixed<$T, FRAC>> for $Int
        where
            Fixed<$T, FRAC>: PartialEq<$Int>,
        {
            fn eq(&self, other: &FastFixed<$T, FRAC>) -> bool {
                other.0 == *self
            }
        }

        impl<const FRAC: usize> PartialOrd<$Int> for FastFixed<$T, FRAC>
        where
            Fixed<$T, FRAC>: PartialOrd<$Int>,
        {
            fn partial_cmp(&self, other: &$Int) -> Option<core::cmp::Ordering> {
                self.0.partial_cmp(other)
            }
        }

        impl<const FRAC: usize> PartialOrd<FastFixed<$T, FRAC>> for $Int
        where
            Fixed<$T, FRAC>: PartialOrd<$Int>,
        {
            fn partial_cmp(&self, other: &FastFixed<$T, FRAC>) -> Option<core::cmp::Ordering> {
                other.0.partial_cmp(self).map(|o| o.reverse())
            }
        }
    };
}

macro_rules! gen_fast_fixed_integer_comparisons {
    ($label:ident: [$($T:ty),*], $Int_list:tt) => {
        $(
            gen_fast_fixed_integer_comparisons!(@one $T, $Int_list);
        )*
    };
    (@one $T:ty, [$($Int:ty),*]) => {
        $(
            impl_fast_fixed_integer_comparison!($T, $Int);
        )*
    };
}

gen_fast_fixed_integer_comparisons!(signed: [i8, i16, i32, i64], [i8, i16, i32, i64]);
gen_fast_fixed_integer_comparisons!(unsigned: [u8, u16, u32, u64], [u8, u16, u32, u64]);
gen_fast_fixed_integer_comparisons!(mixed: [i8, i16, i32, i64], [u8, u16, u32, u64]);
gen_fast_fixed_integer_comparisons!(mixed: [u8, u16, u32, u64], [i8, i16, i32, i64]);

// Commutative operators between FastFixed and raw integers
macro_rules! impl_fast_fixed_integer_arithmetic {
    ($T:ty, $Int:ty) => {
        impl<const FRAC: usize> core::ops::Add<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteAddSub<$Int> + num_traits::NumCast + Copy,
            $Int: num_traits::NumCast + Copy,
            <$T as PromoteAddSub<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>;

            fn add(self, rhs: $Int) -> Self::Output {
                type Out = <$T as PromoteAddSub<$Int>>::Output;
                type I64 = <Out as FastInt>::Intermediate;
                let val_l: I64 = clamp_cast(self.0.raw_value());
                let val_r: I64 = clamp_cast(rhs);
                let val_r_shifted = saturating_shl(val_r, FRAC);
                let sum = val_l.saturating_add(val_r_shifted);
                FastFixed(Fixed::from_raw(clamp_cast(sum)))
            }
        }

        impl<const FRAC: usize> core::ops::Add<FastFixed<$T, FRAC>> for $Int
        where
            $T: PromoteAddSub<$Int> + num_traits::NumCast + Copy,
            $Int: num_traits::NumCast + Copy,
            <$T as PromoteAddSub<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>;

            fn add(self, rhs: FastFixed<$T, FRAC>) -> Self::Output {
                type Out = <$T as PromoteAddSub<$Int>>::Output;
                type I64 = <Out as FastInt>::Intermediate;
                let val_l: I64 = clamp_cast(self);
                let val_r: I64 = clamp_cast(rhs.0.raw_value());
                let val_l_shifted = saturating_shl(val_l, FRAC);
                let sum = val_l_shifted.saturating_add(val_r);
                FastFixed(Fixed::from_raw(clamp_cast(sum)))
            }
        }

        impl<const FRAC: usize> core::ops::Sub<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteAddSub<$Int> + num_traits::NumCast + Copy,
            $Int: num_traits::NumCast + Copy,
            <$T as PromoteAddSub<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>;

            fn sub(self, rhs: $Int) -> Self::Output {
                type Out = <$T as PromoteAddSub<$Int>>::Output;
                type I64 = <Out as FastInt>::Intermediate;
                let val_l: I64 = clamp_cast(self.0.raw_value());
                let val_r: I64 = clamp_cast(rhs);
                let val_r_shifted = saturating_shl(val_r, FRAC);
                let diff = val_l.saturating_sub(val_r_shifted);
                FastFixed(Fixed::from_raw(clamp_cast(diff)))
            }
        }

        impl<const FRAC: usize> core::ops::Sub<FastFixed<$T, FRAC>> for $Int
        where
            $T: PromoteAddSub<$Int> + num_traits::NumCast + Copy,
            $Int: num_traits::NumCast + Copy,
            <$T as PromoteAddSub<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>;

            fn sub(self, rhs: FastFixed<$T, FRAC>) -> Self::Output {
                type Out = <$T as PromoteAddSub<$Int>>::Output;
                type I64 = <Out as FastInt>::Intermediate;
                let val_l: I64 = clamp_cast(self);
                let val_r: I64 = clamp_cast(rhs.0.raw_value());
                let val_l_shifted = saturating_shl(val_l, FRAC);
                let diff = val_l_shifted.saturating_sub(val_r);
                FastFixed(Fixed::from_raw(clamp_cast(diff)))
            }
        }

        impl<const FRAC: usize> core::ops::Mul<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteMul<$Int> + num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            $Int: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            <$T as PromoteMul<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteMul<$Int>>::Output, FRAC>;

            fn mul(self, rhs: $Int) -> Self::Output {
                type Out = <$T as PromoteMul<$Int>>::Output;
                type I64 = <Out as FastInt>::Intermediate;
                let val_l: I64 = clamp_cast(self.0.raw_value());
                let val_r: I64 = clamp_cast(rhs);
                let prod = val_l.saturating_mul(val_r);
                FastFixed(Fixed::from_raw(clamp_cast(prod)))
            }
        }

        impl<const FRAC: usize> core::ops::Mul<FastFixed<$T, FRAC>> for $Int
        where
            $T: PromoteMul<$Int> + num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            $Int: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            <$T as PromoteMul<$Int>>::Output:
                num_traits::Bounded + num_traits::NumCast + Copy + FastInt,
        {
            type Output = FastFixed<<$T as PromoteMul<$Int>>::Output, FRAC>;

            fn mul(self, rhs: FastFixed<$T, FRAC>) -> Self::Output {
                rhs.mul(self)
            }
        }

        impl<const FRAC: usize> core::ops::AddAssign<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteAddSub<$Int>
                + num_traits::Zero
                + PartialOrd
                + num_traits::Bounded
                + num_traits::NumCast
                + Copy
                + FastInt,
            <$T as PromoteAddSub<$Int>>::Output: FastInt,
            $Int: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            FastFixed<$T, FRAC>:
                core::ops::Add<$Int, Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>>,
        {
            fn add_assign(&mut self, rhs: $Int) {
                let sum = *self + rhs;
                self.0 = Fixed::from_raw(clamp_cast(sum.0.raw_value()));
            }
        }

        impl<const FRAC: usize> core::ops::SubAssign<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteAddSub<$Int>
                + num_traits::Zero
                + PartialOrd
                + num_traits::Bounded
                + num_traits::NumCast
                + Copy
                + FastInt,
            <$T as PromoteAddSub<$Int>>::Output: FastInt,
            $Int: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            FastFixed<$T, FRAC>:
                core::ops::Sub<$Int, Output = FastFixed<<$T as PromoteAddSub<$Int>>::Output, FRAC>>,
        {
            fn sub_assign(&mut self, rhs: $Int) {
                let diff = *self - rhs;
                self.0 = Fixed::from_raw(clamp_cast(diff.0.raw_value()));
            }
        }

        impl<const FRAC: usize> core::ops::MulAssign<$Int> for FastFixed<$T, FRAC>
        where
            $T: PromoteMul<$Int>
                + num_traits::Zero
                + PartialOrd
                + num_traits::Bounded
                + num_traits::NumCast
                + Copy
                + FastInt,
            <$T as PromoteMul<$Int>>::Output: FastInt,
            $Int: num_traits::Zero + PartialOrd + num_traits::NumCast + Copy,
            FastFixed<$T, FRAC>:
                core::ops::Mul<$Int, Output = FastFixed<<$T as PromoteMul<$Int>>::Output, FRAC>>,
        {
            fn mul_assign(&mut self, rhs: $Int) {
                let prod = *self * rhs;
                self.0 = Fixed::from_raw(clamp_cast(prod.0.raw_value()));
            }
        }

        impl<const FRAC: usize> core::ops::Div<$Int> for FastFixed<$T, FRAC> {
            type Output = DivExpression<FastFixed<$T, FRAC>, FastFixed<$Int, 0>>;

            fn div(self, rhs: $Int) -> Self::Output {
                DivExpression { numerator: self, denominator: FastFixed(Fixed::from_raw(rhs)) }
            }
        }

        impl<const FRAC: usize> core::ops::DivAssign<$Int> for FastFixed<$T, FRAC>
        where
            Self: Copy,
            FastFixed<$T, FRAC>: From<DivExpression<FastFixed<$T, FRAC>, FastFixed<$Int, 0>>>,
        {
            fn div_assign(&mut self, rhs: $Int) {
                *self = (*self / rhs).into();
            }
        }
    };
}

macro_rules! gen_fast_fixed_integer_arithmetic {
    ($label:ident: [$($T:ty),*], $Int_list:tt) => {
        $(
            gen_fast_fixed_integer_arithmetic!(@one $T, $Int_list);
        )*
    };
    (@one $T:ty, [$($Int:ty),*]) => {
        $(
            impl_fast_fixed_integer_arithmetic!($T, $Int);
        )*
    };
}

gen_fast_fixed_integer_arithmetic!(signed: [i8, i16, i32, i64], [i8, i16, i32, i64]);
gen_fast_fixed_integer_arithmetic!(unsigned: [u8, u16, u32, u64], [u8, u16, u32, u64]);
gen_fast_fixed_integer_arithmetic!(mixed: [i8, i16, i32, i64], [u8, u16, u32, u64]);
gen_fast_fixed_integer_arithmetic!(mixed: [u8, u16, u32, u64], [i8, i16, i32, i64]);
