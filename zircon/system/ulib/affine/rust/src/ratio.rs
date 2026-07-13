// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zr::static_assert;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exact {
    No,
    Yes,
}

/// Rounding Behaviors used when scaling.
///
/// | val  | N   | D   | Down | Up  | TowardsZero | AwayFromZero |
/// | :--- | :-- | :-- | :--- | :-- | :---------- | :----------- |
/// | 7    | 1   | 2   | 3    | 4   | 3           | 4            |
/// | -7   | 1   | 2   | -4   | -3  | -3          | -4           |
pub struct Round;
impl Round {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
    pub const TOWARDS_ZERO: u8 = 2;
    pub const AWAY_FROM_ZERO: u8 = 3;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    numerator: u32,
    denominator: u32,
}

static_assert!(core::mem::size_of::<Ratio>() == 8);
static_assert!(core::mem::align_of::<Ratio>() == 4);

impl Default for Ratio {
    fn default() -> Self {
        Ratio { numerator: 1, denominator: 1 }
    }
}

impl Ratio {
    pub const OVERFLOW: i64 = i64::MAX;
    pub const UNDERFLOW: i64 = i64::MIN;

    pub fn new(numerator: u32, denominator: u32) -> Self {
        debug_assert!(denominator != 0);
        Ratio { numerator, denominator }
    }

    pub fn numerator(&self) -> u32 {
        self.numerator
    }

    pub fn denominator(&self) -> u32 {
        self.denominator
    }

    pub fn invertible(&self) -> bool {
        self.numerator != 0
    }

    pub fn inverse(&self) -> Self {
        debug_assert!(self.invertible());
        Ratio { numerator: self.denominator, denominator: self.numerator }
    }

    /// Reduces the ratio of numerator/denominator in-place (32-bit).
    pub fn reduce_u32(numerator: &mut u32, denominator: &mut u32) {
        assert!(*denominator != 0);
        if *numerator == 0 {
            *denominator = 1;
            return;
        }
        let gcd = binary_gcd(*numerator as u64, *denominator as u64) as u32;
        *numerator /= gcd;
        *denominator /= gcd;
    }

    /// Reduces the ratio of numerator/denominator in-place (64-bit).
    pub fn reduce_u64(numerator: &mut u64, denominator: &mut u64) {
        assert!(*denominator != 0);
        if *numerator == 0 {
            *denominator = 1;
            return;
        }
        let gcd = binary_gcd(*numerator, *denominator);
        *numerator /= gcd;
        *denominator /= gcd;
    }

    /// Reduces the ratio instance in-place.
    pub fn reduce(&mut self) {
        Self::reduce_u32(&mut self.numerator, &mut self.denominator);
    }

    /// Produces the product of two ratios.
    ///
    /// If `exact` is `Exact::Yes`, this panics on loss of precision.
    /// If `exact` is `Exact::No`, it attempts to find the best 32-bit approximation.
    pub fn product_raw(
        a_numerator: u32,
        a_denominator: u32,
        b_numerator: u32,
        b_denominator: u32,
        exact: Exact,
    ) -> (u32, u32) {
        let mut numerator = a_numerator as u64 * b_numerator as u64;
        let mut denominator = a_denominator as u64 * b_denominator as u64;

        Self::reduce_u64(&mut numerator, &mut denominator);

        if numerator > u32::MAX as u64 || denominator > u32::MAX as u64 {
            assert!(exact == Exact::No, "Precision loss in exact Ratio::product");

            // Try to find the best approximation of the ratio that we can. Our
            // approach is as follows. Figure out the number of bits to the right
            // we need to shift the numerator and denominator, rounding up or down
            // in the process, such that the result can be reduced to fit into 32
            // bits.
            //
            // This approach tends to beat out a just-shift-until-it-fits approach,
            // as well as an always-shift-then-reduce approach, but _none_ of these
            // approaches always finds the best solution.
            //
            // TODO(johngro): figure out if it is reasonable to actually compute
            // the best solution. Alternatively, consider implementing a "just
            // shift until it fits" solution if the approximate results are good
            // enough.
            for i in 1..=32 {
                // Produce a version of the numerator and denominator which have
                // each been divided by 2^i, rounding up/down as appropriate
                // (instead of truncating).
                let rounded_numerator = (numerator + (1u64 << (i - 1))) >> i;
                let rounded_denominator = (denominator + (1u64 << (i - 1))) >> i;

                if rounded_denominator == 0 {
                    // Product is larger than we can represent. Return the largest value we
                    // can represent.
                    return (u32::MAX, 1);
                }

                if rounded_numerator == 0 {
                    // Product is smaller than we can represent. Return 0.
                    return (0, 1);
                }

                let mut rn = rounded_numerator;
                let mut rd = rounded_denominator;
                Self::reduce_u64(&mut rn, &mut rd);
                if rn <= u32::MAX as u64 && rd <= u32::MAX as u64 {
                    return (rn as u32, rd as u32);
                }
            }
            // Fallback (should be unreachable)
            return (numerator as u32, denominator as u32);
        }

        (numerator as u32, denominator as u32)
    }

    pub fn product(a: Ratio, b: Ratio, exact: Exact) -> Ratio {
        let (n, d) =
            Self::product_raw(a.numerator, a.denominator, b.numerator, b.denominator, exact);
        Ratio { numerator: n, denominator: d }
    }

    /// Scales an `i64` value by the ratio of numerator/denominator.
    ///
    /// Returns a saturated value (`OVERFLOW` or `UNDERFLOW`) on overflow/underflow.
    /// The rounding behavior is determined by the `ROUND` const generic.
    pub fn scale_with_round<const ROUND: u8>(value: i64, numerator: u32, denominator: u32) -> i64 {
        assert!(denominator != 0);

        if value >= 0 {
            // LIMIT == 0x7FFFFFFFFFFFFFFF
            let limit = i64::MAX as u64;
            let round_up = match ROUND {
                Round::UP | Round::AWAY_FROM_ZERO => true,
                _ => false,
            };
            let scaled = scale_unsigned(value as u64, numerator, denominator, round_up, limit);
            scaled as i64
        } else {
            // LIMIT == 0x8000000000000000
            //
            // Note:  We are attempting to pass the unsigned distance from zero into
            // our ScaleUInt64 function.  In the case of negative numbers, we pass
            // the twos compliment into the scale function, and then flip the sign
            // again on the way out.
            //
            // We are taking the advantage of the fact that the twos compliment of
            // MIN is itself for any signed integer type, and that casting this
            // value to an unsigned integer of the same size properly produces the
            // original value's distance from zero.  Clamping the limit to the
            // distance of MIN from zero means that saturated results will likewise
            // get properly flipped back to MIN during the return.
            //
            let limit = 0x8000000000000000u64; // i64::MIN.unsigned_abs()
            let round_up = match ROUND {
                Round::DOWN | Round::AWAY_FROM_ZERO => true,
                _ => false,
            };
            let scaled =
                scale_unsigned(value.unsigned_abs(), numerator, denominator, round_up, limit);
            if scaled == 0x8000000000000000 { i64::MIN } else { -(scaled as i64) }
        }
    }

    /// Scales an `i64` value by this ratio.
    ///
    /// Returns a saturated value (`OVERFLOW` or `UNDERFLOW`) on overflow/underflow.
    /// The rounding behavior is determined by the `ROUND` const generic.
    pub fn scale<const ROUND: u8>(&self, value: i64) -> i64 {
        Self::scale_with_round::<ROUND>(value, self.numerator, self.denominator)
    }
}

// Calculates the greatest common denominator (factor) of two values.
fn binary_gcd(mut a: u64, mut b: u64) -> u64 {
    debug_assert!(a != 0 && b != 0);

    // Remove and count the common factors of 2.
    let mut twos = 0;
    while ((a | b) & 1) == 0 {
        a >>= 1;
        b >>= 1;
        twos += 1;
    }

    // Get rid of the non-common factors of 2 in a. a is non-zero, so this
    // terminates.
    while (a & 1) == 0 {
        a >>= 1;
    }

    loop {
        // Get rid of the non-common factors of 2 in b. b is non-zero, so this
        // terminates.
        while (b & 1) == 0 {
            b >>= 1;
        }

        // Apply the Euclid subtraction method.
        if a > b {
            core::mem::swap(&mut a, &mut b);
        }

        b -= a;
        if b == 0 {
            break;
        }
    }

    // Multiply in the common factors of two.
    a << twos
}

// Scales a uint64_t value by the ratio of two uint32_t values. If round_up is
// true, the result is rounded up rather than down. Saturates at `limit` on overflow.
fn scale_unsigned(value: u64, numerator: u32, denominator: u32, round_up: bool, limit: u64) -> u64 {
    let prod = (value as u128) * (numerator as u128);
    let q = prod / (denominator as u128);
    let r = prod % (denominator as u128);

    if q >= (limit as u128) {
        return limit;
    }

    let mut result = q as u64;
    if round_up && r != 0 {
        result += 1;
        if result >= limit {
            return limit;
        }
    }
    result
}

// Operators

impl core::ops::Mul for Ratio {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self::Output {
        Ratio::product(self, rhs, Exact::Yes)
    }
}

impl core::ops::Div for Ratio {
    type Output = Self;
    fn div(self, rhs: Self) -> Self::Output {
        self * rhs.inverse()
    }
}

impl core::ops::Mul<i64> for Ratio {
    type Output = i64;
    fn mul(self, rhs: i64) -> Self::Output {
        self.scale::<{ Round::DOWN }>(rhs)
    }
}

impl core::ops::Mul<Ratio> for i64 {
    type Output = i64;
    fn mul(self, rhs: Ratio) -> Self::Output {
        rhs.scale::<{ Round::DOWN }>(self)
    }
}

impl core::ops::Div<Ratio> for i64 {
    type Output = i64;
    fn div(self, rhs: Ratio) -> Self::Output {
        rhs.inverse().scale::<{ Round::DOWN }>(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_construction() {
        let valid_vectors = [(0, 1), (1, 1), (23, 41)];
        for &(n, d) in &valid_vectors {
            let r = Ratio::new(n, d);
            assert_eq!(r.numerator(), n);
            assert_eq!(r.denominator(), d);
        }

        // Ratio::default() produces 1/1
        let r = Ratio::default();
        assert_eq!(r.numerator(), 1);
        assert_eq!(r.denominator(), 1);

        // Reduction is NOT automatically performed
        let r = Ratio::new(9, 21);
        assert_eq!(r.numerator(), 9);
        assert_eq!(r.denominator(), 21);
    }

    #[test]
    fn test_reduction_32() {
        let mut vectors = [
            (1, 1, 1, 1),
            (10, 10, 1, 1),
            (10, 2, 5, 1),
            (0, 1, 0, 1),
            (0, 500, 0, 1),
            (48000, 44100, 160, 147),
            (44100, 48000, 147, 160),
            (1000007, 1000000, 1000007, 1000000),
        ];

        for v in &mut vectors {
            let mut n = v.0;
            let mut d = v.1;
            Ratio::reduce_u32(&mut n, &mut d);
            assert_eq!((n, d), (v.2, v.3));

            let mut r = Ratio::new(v.0, v.1);
            r.reduce();
            assert_eq!((r.numerator(), r.denominator()), (v.2, v.3));
        }
    }

    #[test]
    fn test_reduction_64() {
        let mut vectors = [
            (1, 1, 1, 1),
            (10, 10, 1, 1),
            (10, 2, 5, 1),
            (0, 1, 0, 1),
            (0, 500, 0, 1),
            (48000, 44100, 160, 147),
            (44100, 48000, 147, 160),
            (1000007, 1000000, 1000007, 1000000),
            (48000336000, 44100000000, 1000007, 918750),
        ];

        for v in &mut vectors {
            let mut n = v.0;
            let mut d = v.1;
            Ratio::reduce_u64(&mut n, &mut d);
            assert_eq!((n, d), (v.2, v.3));
        }
    }

    #[test]
    fn test_product() {
        struct TestVector {
            a_n: u32,
            a_d: u32,
            b_n: u32,
            b_d: u32,
            expected_n: u32,
            expected_d: u32,
            exact: Exact,
        }

        let test_vectors = [
            TestVector {
                a_n: 1,
                a_d: 1,
                b_n: 1,
                b_d: 1,
                expected_n: 1,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 0,
                a_d: 1,
                b_n: 1,
                b_d: 1,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 0,
                a_d: 500,
                b_n: 1,
                b_d: 1,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 3,
                a_d: 4,
                b_n: 5,
                b_d: 9,
                expected_n: 5,
                expected_d: 12,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 48000,
                a_d: 44100,
                b_n: 1000007,
                b_d: 1000000,
                expected_n: 1000007,
                expected_d: 918750,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 3465653567,
                a_d: 2327655023,
                b_n: 1291540343,
                b_d: 3698423317,
                expected_n: 317609835,
                expected_d: 610852072,
                exact: Exact::No,
            },
            TestVector {
                a_n: 0xFFFFFFFF,
                a_d: 1,
                b_n: 0xFFFFFFFF,
                b_d: 1,
                expected_n: 0xFFFFFFFF,
                expected_d: 1,
                exact: Exact::No,
            },
            TestVector {
                a_n: 1,
                a_d: 0xFFFFFFFF,
                b_n: 1,
                b_d: 0xFFFFFFFF,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::No,
            },
        ];

        for v in &test_vectors {
            let a = Ratio::new(v.a_n, v.a_d);
            let b = Ratio::new(v.b_n, v.b_d);

            let res = Ratio::product(a, b, v.exact);
            assert_eq!(
                (res.numerator(), res.denominator()),
                (v.expected_n, v.expected_d),
                "Expected {}/{} * {}/{} to produce {}/{}; got {}/{} instead (static)",
                v.a_n,
                v.a_d,
                v.b_n,
                v.b_d,
                v.expected_n,
                v.expected_d,
                res.numerator(),
                res.denominator()
            );

            let res = Ratio::product(b, a, v.exact);
            assert_eq!(
                (res.numerator(), res.denominator()),
                (v.expected_n, v.expected_d),
                "Expected {}/{} * {}/{} to produce {}/{}; got {}/{} instead (commutative static)",
                v.b_n,
                v.b_d,
                v.a_n,
                v.a_d,
                v.expected_n,
                v.expected_d,
                res.numerator(),
                res.denominator()
            );

            if v.exact == Exact::Yes {
                let res = a * b;
                assert_eq!((res.numerator(), res.denominator()), (v.expected_n, v.expected_d));

                let res = b * a;
                assert_eq!((res.numerator(), res.denominator()), (v.expected_n, v.expected_d));

                if b.invertible() {
                    let res = a / b.inverse();
                    assert_eq!((res.numerator(), res.denominator()), (v.expected_n, v.expected_d));
                }

                if a.invertible() {
                    let res = b / a.inverse();
                    assert_eq!((res.numerator(), res.denominator()), (v.expected_n, v.expected_d));
                }
            }
        }
    }

    #[test]
    fn test_product_raw() {
        struct TestVector {
            a_n: u32,
            a_d: u32,
            b_n: u32,
            b_d: u32,
            expected_n: u32,
            expected_d: u32,
            exact: Exact,
        }

        let test_vectors = [
            TestVector {
                a_n: 1,
                a_d: 1,
                b_n: 1,
                b_d: 1,
                expected_n: 1,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 0,
                a_d: 1,
                b_n: 1,
                b_d: 1,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 0,
                a_d: 500,
                b_n: 1,
                b_d: 1,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 3,
                a_d: 4,
                b_n: 5,
                b_d: 9,
                expected_n: 5,
                expected_d: 12,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 48000,
                a_d: 44100,
                b_n: 1000007,
                b_d: 1000000,
                expected_n: 1000007,
                expected_d: 918750,
                exact: Exact::Yes,
            },
            TestVector {
                a_n: 3465653567,
                a_d: 2327655023,
                b_n: 1291540343,
                b_d: 3698423317,
                expected_n: 317609835,
                expected_d: 610852072,
                exact: Exact::No,
            },
            TestVector {
                a_n: 0xFFFFFFFF,
                a_d: 1,
                b_n: 0xFFFFFFFF,
                b_d: 1,
                expected_n: 0xFFFFFFFF,
                expected_d: 1,
                exact: Exact::No,
            },
            TestVector {
                a_n: 1,
                a_d: 0xFFFFFFFF,
                b_n: 1,
                b_d: 0xFFFFFFFF,
                expected_n: 0,
                expected_d: 1,
                exact: Exact::No,
            },
        ];

        for v in &test_vectors {
            let res = Ratio::product_raw(v.a_n, v.a_d, v.b_n, v.b_d, v.exact);
            assert_eq!(
                res,
                (v.expected_n, v.expected_d),
                "Expected {}/{} * {}/{} to produce {}/{}; got {}/{} instead",
                v.a_n,
                v.a_d,
                v.b_n,
                v.b_d,
                v.expected_n,
                v.expected_d,
                res.0,
                res.1
            );
        }
    }

    fn test_scale_helper<const ROUND: u8>() {
        struct TestVector {
            val: i64,
            n: u32,
            d: u32,
            expected: i64,
            fractional_result: bool,
        }

        let test_vectors = [
            TestVector { val: 0, n: 0, d: 1, expected: 0, fractional_result: false },
            TestVector { val: 1234567890, n: 0, d: 1, expected: 0, fractional_result: false },
            TestVector { val: 0, n: 1, d: 1, expected: 0, fractional_result: false },
            TestVector {
                val: 1234567890,
                n: 1,
                d: 1,
                expected: 1234567890,
                fractional_result: false,
            },
            TestVector { val: 198, n: 48000, d: 44100, expected: 215, fractional_result: true },
            TestVector { val: -198, n: 48000, d: 44100, expected: -216, fractional_result: true },
            TestVector {
                val: 49 * 198,
                n: 48000,
                d: 44100,
                expected: 10560,
                fractional_result: false,
            },
            TestVector {
                val: -(49 * 198),
                n: 48000,
                d: 44100,
                expected: -10560,
                fractional_result: false,
            },
            TestVector {
                val: (49 * 198) + 1,
                n: 48000,
                d: 44100,
                expected: 10561,
                fractional_result: true,
            },
            TestVector {
                val: -((49 * 198) + 1),
                n: 48000,
                d: 44100,
                expected: -10562,
                fractional_result: true,
            },
            TestVector {
                val: 0x1517ffffeae80,
                n: 0xbebc200,
                d: 0x33333333,
                expected: 0x4e94914f0000,
                fractional_result: false,
            },
            TestVector {
                val: -0x1517ffffeae80,
                n: 0xbebc200,
                d: 0x33333333,
                expected: -0x4e94914f0000,
                fractional_result: false,
            },
            TestVector {
                val: i64::MAX,
                n: 1000001,
                d: 1000000,
                expected: Ratio::OVERFLOW,
                fractional_result: false,
            },
            TestVector {
                val: i64::MIN,
                n: 1000001,
                d: 1000000,
                expected: Ratio::UNDERFLOW,
                fractional_result: false,
            },
            TestVector {
                val: -0x2000000000000001,
                n: 4,
                d: 1,
                expected: Ratio::UNDERFLOW,
                fractional_result: false,
            },
        ];

        for v in &test_vectors {
            let res_static = Ratio::scale_with_round::<ROUND>(v.val, v.n, v.d);
            let r = Ratio::new(v.n, v.d);
            let res_inst = r.scale::<ROUND>(v.val);

            let adjusted_expected = if !v.fractional_result || ROUND == Round::DOWN {
                v.expected
            } else if v.val >= 0 {
                if ROUND == Round::TOWARDS_ZERO { v.expected } else { v.expected + 1 }
            } else {
                if ROUND == Round::AWAY_FROM_ZERO { v.expected } else { v.expected + 1 }
            };

            assert_eq!(
                res_static, adjusted_expected,
                "Static: Expected {} * {}/{} to produce {}; got {}",
                v.val, v.n, v.d, adjusted_expected, res_static
            );
            assert_eq!(
                res_inst, adjusted_expected,
                "Instanced: Expected {} * {}/{} to produce {}; got {}",
                v.val, v.n, v.d, adjusted_expected, res_inst
            );

            if ROUND == Round::DOWN {
                let res_op1 = r * v.val;
                let res_op2 = v.val * r;
                assert_eq!(res_op1, adjusted_expected);
                assert_eq!(res_op2, adjusted_expected);

                if r.invertible() {
                    let res_op3 = v.val / r.inverse();
                    assert_eq!(res_op3, adjusted_expected);
                }
            }
        }
    }

    #[test]
    fn test_scale_round_down() {
        test_scale_helper::<{ Round::DOWN }>();
    }
    #[test]
    fn test_scale_round_up() {
        test_scale_helper::<{ Round::UP }>();
    }
    #[test]
    fn test_scale_round_towards_zero() {
        test_scale_helper::<{ Round::TOWARDS_ZERO }>();
    }
    #[test]
    fn test_scale_round_away_from_zero() {
        test_scale_helper::<{ Round::AWAY_FROM_ZERO }>();
    }

    #[test]
    fn test_inverse() {
        let test_vectors = [(1, 1), (123456, 987654)];
        for &(n, d) in &test_vectors {
            let r = Ratio::new(n, d);
            let inv = r.inverse();
            assert_eq!(inv.numerator(), d);
            assert_eq!(inv.denominator(), n);
        }

        let r = Ratio::new(0, 1);
        assert!(!r.invertible());
    }
}
