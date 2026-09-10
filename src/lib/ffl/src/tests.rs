// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::exponential_average::ExponentialAverage;
use crate::fixed::Fixed;

#[test]
fn test_exponential_average_single_rate() {
    let initial_value = Fixed::<i64, 0>::from_integer(1_000_000);
    let alpha = Fixed::<i32, 2>::from_ratio(1, 2);
    let mut avg = ExponentialAverage::new_single_rate(initial_value, alpha);
    assert_eq!(avg.value().raw_value(), initial_value.raw_value());

    struct TestCase {
        sample: Fixed<i64, 0>,
        expected_avg: Fixed<i64, 0>,
        expected_variance: Fixed<i64, 0>,
    }
    let test_cases = vec![
        TestCase {
            sample: Fixed::from_integer(1_000_000),
            expected_avg: Fixed::from_integer(1_000_000),
            expected_variance: Fixed::from_integer(0),
        },
        TestCase {
            sample: Fixed::from_integer(2_000_000),
            expected_avg: Fixed::from_integer(1_500_000),
            expected_variance: Fixed::from_integer(250_000_000_000),
        },
        TestCase {
            sample: Fixed::from_integer(5_000_000),
            expected_avg: Fixed::from_integer(3_250_000),
            expected_variance: Fixed::from_integer(3_187_500_000_000),
        },
        TestCase {
            sample: Fixed::from_integer(3_000_000),
            expected_avg: Fixed::from_integer(3_125_000),
            expected_variance: Fixed::from_integer(1_609_375_000_000),
        },
        TestCase {
            sample: Fixed::from_integer(1_000_000),
            expected_avg: Fixed::from_integer(2_062_500),
            expected_variance: Fixed::from_integer(1_933_593_750_000),
        },
    ];

    for tc in test_cases {
        avg.add_sample(tc.sample);
        assert_eq!(avg.value().raw_value(), tc.expected_avg.raw_value());
        assert_eq!(avg.variance().raw_value(), tc.expected_variance.raw_value());
    }
}

#[test]
fn test_exponential_average_dual_rate() {
    let alpha = Fixed::<i32, 2>::from_ratio(1, 2);
    let beta = Fixed::<i32, 2>::from_ratio(1, 4);
    let initial_value = Fixed::<i64, 0>::from_integer(1_000_000);
    let mut avg = ExponentialAverage::new(initial_value, alpha, beta);
    assert_eq!(avg.value().raw_value(), initial_value.raw_value());

    struct TestCase {
        sample: Fixed<i64, 0>,
        expected_avg: Fixed<i64, 0>,
        expected_variance: Fixed<i64, 0>,
    }
    let test_cases = vec![
        TestCase {
            sample: Fixed::from_integer(2_000_000),
            expected_avg: Fixed::from_integer(1_500_000),
            expected_variance: Fixed::from_integer(250_000_000_000),
        },
        TestCase {
            sample: Fixed::from_integer(5_000_000),
            expected_avg: Fixed::from_integer(3_250_000),
            expected_variance: Fixed::from_integer(3_187_500_000_000),
        },
        TestCase {
            sample: Fixed::from_integer(1_000_000),
            expected_avg: Fixed::from_integer(2_687_500),
            expected_variance: Fixed::from_integer(3_339_843_750_000),
        },
        TestCase {
            sample: Fixed::from_integer(1_000_000),
            expected_avg: Fixed::from_integer(2_265_625),
            expected_variance: Fixed::from_integer(3_038_818_359_375),
        },
    ];

    for tc in test_cases {
        avg.add_sample(tc.sample);
        assert_eq!(avg.value().raw_value(), tc.expected_avg.raw_value());
        assert_eq!(avg.variance().raw_value(), tc.expected_variance.raw_value());
    }
}

#[test]
fn test_saturating_arithmetic() {
    use crate::saturating_arithmetic::{saturate_add, saturate_multiply, saturate_subtract};

    // Test i8 + i8
    assert_eq!(saturate_add::<i8, i8, i8>(i8::MAX, 1), i8::MAX);
    assert_eq!(saturate_add::<i8, i8, i8>(i8::MAX, -1), i8::MAX - 1);
    assert_eq!(saturate_add::<i8, i8, i8>(i8::MIN, 1), i8::MIN + 1);
    assert_eq!(saturate_add::<i8, i8, i8>(i8::MIN, -1), i8::MIN);

    // Test u8 + u8
    assert_eq!(saturate_add::<u8, u8, u8>(u8::MAX, 1), u8::MAX);

    // Test mix
    assert_eq!(saturate_add::<i8, i8, u8>(i8::MAX, 1), 128);
    assert_eq!(saturate_add::<i8, i8, u8>(i8::MIN, 1), 0);

    // Test subtraction
    assert_eq!(saturate_subtract::<i8, i8, i8>(i8::MAX, 1), i8::MAX - 1);
    assert_eq!(saturate_subtract::<i8, i8, i8>(i8::MAX, -1), i8::MAX);
    assert_eq!(saturate_subtract::<i8, i8, i8>(i8::MIN, 1), i8::MIN);
    assert_eq!(saturate_subtract::<i8, i8, i8>(i8::MIN, -1), i8::MIN + 1);

    // Test mix subtraction
    assert_eq!(saturate_subtract::<i8, i8, u8>(i8::MAX, 1), 126);
    assert_eq!(saturate_subtract::<i8, i8, u8>(i8::MAX, -1), 128);

    // Test multiplication
    assert_eq!(saturate_multiply::<i8, i8, i8>(i8::MAX, 2), i8::MAX);
    assert_eq!(saturate_multiply::<i8, i8, i8>(i8::MIN, 2), i8::MIN);
    assert_eq!(saturate_multiply::<i8, i8, i8>(i8::MAX, -2), i8::MIN);
    assert_eq!(saturate_multiply::<i8, i8, i8>(i8::MIN, -2), i8::MAX);

    // Test u64 multiplication boundary
    assert_eq!(saturate_multiply::<u64, u64, u64>(u64::MAX, 2), u64::MAX);
    assert_eq!(saturate_multiply::<u64, u64, u64>(u64::MAX, u64::MAX), u64::MAX);
}

#[test]
fn test_methods() {
    // Ceiling
    assert_eq!(Fixed::<i32, 0>::from_integer(-1).ceiling(), -1);
    assert_eq!(Fixed::<i32, 1>::from_raw(-1).ceiling(), 0);
    assert_eq!(Fixed::<i32, 2>::from_raw(-8).ceiling(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-7).ceiling(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-5).ceiling(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-4).ceiling(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-2).ceiling(), 0);

    // Floor
    assert_eq!(Fixed::<i32, 0>::from_integer(-1).floor(), -1);
    assert_eq!(Fixed::<i32, 1>::from_raw(-1).floor(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-8).floor(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-7).floor(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-5).floor(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-4).floor(), -1);

    // Round
    assert_eq!(Fixed::<i32, 0>::from_integer(-1).round(), -1);
    assert_eq!(Fixed::<i32, 1>::from_integer(-1).round(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-8).round(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-7).round(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-6).round(), -2);
    assert_eq!(Fixed::<i32, 2>::from_raw(-5).round(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-4).round(), -1);
    assert_eq!(Fixed::<i32, 2>::from_raw(-2).round(), 0);
    assert_eq!(Fixed::<i32, 2>::from_raw(-1).round(), 0);
    assert_eq!(Fixed::<i32, 1>::from_raw(-1).round(), 0);

    // Integral / Fraction / Absolute
    let x = Fixed::<i32, 2>::from_raw(-9);
    assert_eq!(x.integral().raw_value(), -8);
    assert_eq!(x.fraction().raw_value(), -1);
    assert_eq!(x.absolute().raw_value(), 9);
}

#[test]
fn test_decimal_string() {
    // Test Decimal Output parity with C++ test suite
    assert_eq!(format!("{:.1}", Fixed::<u8, 0>::min()), "0.0");
    assert_eq!(format!("{:.4}", Fixed::<u8, 4>::max()), "15.9375");
    assert_eq!(format!("{:.8}", Fixed::<u8, 8>::max()), "0.99609375");
    assert_eq!(format!("{:.1}", Fixed::<i8, 0>::min()), "-128.0");
    assert_eq!(format!("{:.4}", Fixed::<i8, 4>::min()), "-8.0000");
    assert_eq!(format!("{:.7}", Fixed::<i8, 7>::max()), "0.9921875");

    assert_eq!(format!("{:.1}", Fixed::<u16, 0>::min()), "0.0");
    assert_eq!(format!("{:.8}", Fixed::<u16, 8>::max()), "255.99609375");
    assert_eq!(format!("{:.10}", Fixed::<u16, 16>::max()), "0.9999847412");
    assert_eq!(format!("{:.1}", Fixed::<i16, 0>::min()), "-32768.0");

    assert_eq!(format!("{:.1}", Fixed::<u32, 0>::min()), "0.0");
    assert_eq!(format!("{:.10}", Fixed::<u32, 16>::max()), "65535.9999847412");
    assert_eq!(format!("{:.10}", Fixed::<u32, 32>::max()), "0.9999999997");
    assert_eq!(format!("{:.1}", Fixed::<i32, 0>::min()), "-2147483648.0");

    assert_eq!(format!("{:.1}", Fixed::<u64, 0>::min()), "0.0");
    assert_eq!(format!("{:.10}", Fixed::<u64, 32>::max()), "4294967295.9999999997");
    assert_eq!(format!("{:.10}", Fixed::<u64, 64>::max()), "0.9999999999");
    assert_eq!(format!("{:.1}", Fixed::<i64, 0>::min()), "-9223372036854775808.0");
}

#[test]
fn test_rational_string() {
    assert_eq!(format!("{}", Fixed::<u8, 0>::min().rational()), "0+0/1");
    assert_eq!(format!("{}", Fixed::<u8, 4>::max().rational()), "15+15/16");
    assert_eq!(format!("{}", Fixed::<u8, 8>::max().rational()), "0+255/256");
    assert_eq!(format!("{}", Fixed::<i8, 0>::min().rational()), "-128-0/1");
    assert_eq!(format!("{}", Fixed::<i8, 4>::min().rational()), "-8-0/16");
    assert_eq!(format!("{}", Fixed::<i8, 7>::min().rational()), "-1-0/128");

    assert_eq!(format!("{}", Fixed::<u16, 16>::max().rational()), "0+65535/65536");
    assert_eq!(format!("{}", Fixed::<i16, 15>::max().rational()), "0+32767/32768");

    assert_eq!(format!("{}", Fixed::<u32, 32>::max().rational()), "0+4294967295/4294967296");
    assert_eq!(format!("{}", Fixed::<i32, 31>::max().rational()), "0+2147483647/2147483648");
}

#[test]
fn test_hex_string() {
    assert_eq!(format!("{:x}", Fixed::<u8, 0>::from_raw(0x23)), "23.0");
    assert_eq!(format!("{:x}", Fixed::<u8, 4>::from_raw(0xaa)), "a.a");
    assert_eq!(format!("{:x}", Fixed::<u8, 7>::from_raw(0xff)), "1.fe");
    assert_eq!(format!("{:x}", Fixed::<u8, 8>::from_raw(0xaa)), "0.aa");

    assert_eq!(format!("{:x}", Fixed::<u16, 8>::from_raw(0x3333)), "33.33");
    assert_eq!(format!("{:x}", Fixed::<u16, 15>::from_raw(0xffff)), "1.fffe");

    assert_eq!(format!("{:x}", Fixed::<u32, 16>::from_raw(0x20203030)), "2020.303");
    assert_eq!(format!("{:x}", Fixed::<u32, 31>::from_raw(0xffffffff)), "1.fffffffe");

    assert_eq!(format!("{:x}", Fixed::<u64, 32>::from_raw(0x2020202030303030)), "20202020.3030303");
}

#[test]
fn test_mixed_resolution_operators() {
    let a = Fixed::<i32, 4>::from_integer(3);
    let b = Fixed::<i32, 8>::from_integer(4);

    let sum = a + b;
    assert_eq!(sum.raw_value(), (3 + 4) << 4); // TARGET_FRAC should be 4

    let diff = a - b;
    assert_eq!(diff.raw_value(), (3 - 4) << 4); // TARGET_FRAC should be 4

    let mut assign = a;
    assign += b;
    assert_eq!(assign.raw_value(), 7 << 4);

    assign -= b;
    assert_eq!(assign.raw_value(), 3 << 4);

    // Mixed comparisons
    assert!(a < b);
    assert!(b > a);
    assert!(a != b);
    assert!(a == Fixed::<i32, 8>::from_integer(3));
}
#[test]
fn test_approximate_unit_conversion() {
    // Verify converting from an ApproximateUnit format (Q0.7, FRAC_SRC=7) to Q15.16 (FRAC_DST=16)
    // scales by exactly 2^(16 - 7) = 512 without a 4x scaling error.
    let approx_val = Fixed::<i8, 7>::from_raw(64); // 0.5 in Q0.7
    let converted = Fixed::<i32, 16>::from_fixed(approx_val); // 0.5 in Q15.16 -> 32768
    assert_eq!(converted.raw_value(), 32768);

    // Verify round_half_to_even does not panic when place >= integer bit width
    assert_eq!(crate::utility::round_half_to_even::<i64>(100, 64), 0);
    assert_eq!(crate::utility::round_half_to_even::<i128>(100, 128), 0);

    // Verify round_half_to_even boundary case for MIN:
    assert_eq!(crate::utility::round_half_to_even::<i64>(i64::MIN, 63), i64::MIN);
}

#[test]
fn test_code_review_fixes() {
    // 1. Buffer overflow in formatting: test rational representation of i64::MIN
    let min_val = Fixed::<i64, 0>::min();
    let min_i64_rat = min_val.rational();
    assert_eq!(format!("{}", min_i64_rat), "-9223372036854775808-0/1");
    let max_val = Fixed::<u64, 64>::max();
    let max_u64_rat = max_val.rational();
    assert!(!format!("{}", max_u64_rat).is_empty());

    // 2. Panic in integral(): ones_place not representable in I
    let x = Fixed::<i8, 7>::from_raw(-128);
    assert_eq!(x.integral().raw_value(), -128);
    let y = Fixed::<i8, 7>::from_raw(127);
    assert_eq!(y.integral().raw_value(), 0);

    // 3. Overflow in comparisons: large fractional differences
    let big_frac = Fixed::<u64, 64>::from_raw(u64::MAX);
    let zero_frac = Fixed::<u64, 0>::from_integer(1);
    assert!(big_frac < zero_frac);
    assert!(zero_frac > big_frac);
    assert_ne!(big_frac, zero_frac);

    // 4. Absolute behavior for I::MIN (saturates to I::MAX)
    assert_eq!(Fixed::<i8, 0>::min().absolute().raw_value(), i8::MAX);
    assert_eq!(Fixed::<i16, 0>::min().absolute().raw_value(), i16::MAX);
    assert_eq!(Fixed::<i32, 0>::min().absolute().raw_value(), i32::MAX);
    assert_eq!(Fixed::<i64, 0>::min().absolute().raw_value(), i64::MAX);

    // 5. ExponentialAverage with types smaller than i64 (using promotion rules)
    let initial = Fixed::<i32, 0>::from_integer(100);
    let alpha = Fixed::<i16, 2>::from_ratio(1, 2);
    let mut avg = ExponentialAverage::new_single_rate(initial, alpha);
    avg.add_sample(Fixed::<i32, 0>::from_integer(200));
    assert_eq!(avg.value().raw_value(), 150);

    // 6. Shift overflow safety checks for very large fractional diffs:
    assert_eq!(crate::fixed::compare_raw(-10, 0, -5, 130), core::cmp::Ordering::Less);
    assert_eq!(crate::fixed::compare_raw(-5, 130, -10, 0), core::cmp::Ordering::Greater);

    // 7. Test division overflow (i128::MIN / -1) does not panic and saturates to max
    let div_num = Fixed::<i64, 0>::from_raw(i64::MIN);
    let div_denom = Fixed::<i64, 0>::from_raw(-1);
    let div_res = Fixed::<i64, 1>::from(div_num / div_denom);
    assert_eq!(div_res.raw_value(), i64::MAX);
    assert_eq!(crate::fixed::compare_raw(10, 0, 5, 130), core::cmp::Ordering::Greater);
    assert_eq!(crate::fixed::compare_raw(5, 130, 10, 0), core::cmp::Ordering::Less);
    assert_eq!(crate::utility::saturating_shl_i128(10, 130), i128::MAX);
    assert_eq!(crate::utility::saturating_shl_i128(-10, 130), i128::MIN);

    // 7. Checked multiplication overflow/saturation:
    let max_fixed_l = Fixed::<u64, 0>::from_raw(u64::MAX);
    let max_fixed_r = Fixed::<u64, 0>::from_raw(u64::MAX);
    assert_eq!(max_fixed_l.mul_to::<u64, 0, u64, 0>(max_fixed_r).raw_value(), u64::MAX);
    assert_eq!((max_fixed_l * max_fixed_r).raw_value(), u64::MAX);

    // 8. Correct calculation of INTEGRAL_MIN and INTEGRAL_MAX for Q1.63 (i64 with 63 fractional bits)
    assert_eq!(crate::fixed_format::FixedFormat::<i64, 63>::INTEGRAL_MIN, -1);
    assert_eq!(crate::fixed_format::FixedFormat::<i64, 63>::INTEGRAL_MAX, 0);
    assert_eq!(crate::fixed_format::FixedFormat::<u64, 63>::INTEGRAL_MIN, 0);
    assert_eq!(crate::fixed_format::FixedFormat::<u64, 63>::INTEGRAL_MAX, 1);

    // 9. Saturating shift wrapping protection
    assert_eq!(crate::utility::saturating_shl_i128(2, 126), i128::MAX);
    assert_eq!(crate::utility::saturating_shl_i128(-1, 127), i128::MIN);
    assert_eq!(crate::utility::saturating_shl_i128(-2, 127), i128::MIN);

    // 10. compare_raw wrapping protection (bits shifted out of u128 range but less than 128)
    assert_eq!(crate::fixed::compare_raw(12, 0, 5, 126), core::cmp::Ordering::Greater);
    assert_eq!(crate::fixed::compare_raw(5, 126, 12, 0), core::cmp::Ordering::Less);
    assert_eq!(crate::fixed::compare_raw(-12, 0, -5, 126), core::cmp::Ordering::Less);

    // 11. Intermediate overflow in saturate_add/saturate_subtract
    let sum_sat: u64 = crate::saturating_arithmetic::saturate_add(i128::MAX - 5, 10i128);
    assert_eq!(sum_sat, u64::MAX);
    let sub_sat: i64 = crate::saturating_arithmetic::saturate_subtract(i128::MIN + 5, 10i128);
    assert_eq!(sub_sat, i64::MIN);

    // 12. u64 * u64 multiplication utilizing full u128 intermediate range without premature i128 saturation
    let val_l = Fixed::<u64, 0>::from_raw(u64::MAX);
    let val_r = Fixed::<u64, 64>::from_raw(u64::MAX);
    let res = val_l.mul_to::<u64, 64, u64, 0>(val_r);
    assert_eq!(res.raw_value(), u64::MAX - 1);
    let res_operator = val_l * val_r;
    assert_eq!(res_operator.raw_value(), u64::MAX - 1);

    // 13. Commutative/symmetric integer comparisons
    let my_fixed = Fixed::<i32, 0>::from_integer(5);
    assert_eq!(my_fixed, 5);
    assert_eq!(5, my_fixed);
    assert!(my_fixed < 10);
    assert!(10 > my_fixed);
    assert!(my_fixed > 2);
    assert!(2 < my_fixed);

    // 14. Mixed-signedness comparisons
    let my_signed_fixed = Fixed::<i32, 0>::from_integer(-5);
    let raw_unsigned: u32 = 10;
    assert!(my_signed_fixed < raw_unsigned);
    assert!(raw_unsigned > my_signed_fixed);
    // 15. Fixed multiplication with raw integers
    let mut fixed_val = Fixed::<i32, 4>::from_integer(10);
    assert_eq!(fixed_val * 3i32, Fixed::<i64, 4>::from_integer(30));
    assert_eq!(3i32 * fixed_val, Fixed::<i64, 4>::from_integer(30));

    // Verify mixed-signedness raw integer multiplication for Fixed
    assert_eq!(fixed_val * raw_unsigned, Fixed::<i64, 4>::from_integer(100));
    assert_eq!(raw_unsigned * fixed_val, Fixed::<i64, 4>::from_integer(100));

    // Verify compound assignment MulAssign with raw integers
    fixed_val *= 2i32;
    assert_eq!(fixed_val, Fixed::<i32, 4>::from_integer(20));
}

#[test]
#[should_panic(expected = "Division by zero in DivExpression")]
fn test_division_by_zero() {
    let num = Fixed::<i32, 0>::from_integer(10);
    let denom = Fixed::<i32, 0>::from_integer(0);
    let _res = Fixed::<i32, 0>::from(num / denom);
}
