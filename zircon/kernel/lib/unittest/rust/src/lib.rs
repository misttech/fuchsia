// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![cfg_attr(not(test), no_std)]

// Allows for the test_suite macro to work within this crate's test module.
#[cfg(test)]
extern crate self as unittest;

use core::ffi::c_char;

#[doc(hidden)]
pub use zx_status::Status as __Status;

/// Attribute macro defining suite of unit tests defined as module.
///
/// Tests are idiomatically modeled as functions, and so modules of such
/// functions make for a natural representation as a test suite.
///
/// Tests defined through this attribute are available to be run via `k ut`.
///
/// A test suite module must meet the following criteria:
/// * It must have a one-line docstring. This line becomes the description of
///   the suite on the kernel command-line.
/// * It must consist only of 'test functions' (see below).
/// * It must contain at least one test function.
///
/// A test function must meet the following criteria:
/// * It too must have a one-line docstring. This line becomes the description
///   of the test on the kernel command-line.
/// * It must have a () -> () signature.
///
/// A test function should make assertions only using the declarative
/// assert_*/expect_* macros defined in the unittest module.
///
/// # Example
/// ```rust
/// /// Brief test suite description.
/// #[test_suite]
/// mod my_suite {
///     /// Brief test case description.
///     fn my_case() {
///         assert_false!(false);
///         expect_true!(1 == 1, "expectation with a message");
///     }
/// }
/// ```
///
pub use unittest_macro::test_suite;

// We leverage libc for printing for now. Once all unittests are in Rust we can
// revisit how printing should work here.
unsafe extern "C" {
    pub fn printf(format: *const c_char, ...) -> core::ffi::c_int;
}

#[macro_export]
#[doc(hidden)]
macro_rules! c_str_lit {
    ($s:expr) => {
        concat!($s, "\0").as_ptr() as *const core::ffi::c_char
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! check_comparison {
    ($cond:expr, $early_return:expr, $op:literal, $expected:expr, $expected_val:expr, $actual:expr, $actual_val:expr, $msg:expr) => {
        if !$cond {
            record_failure!();
            let format = $crate::failed_format!(concat!("expected %s (%ld) ", $op, " %s (%ld)"));
            let file_c_str = $crate::c_str_lit!(file!());
            let expected_str = $crate::c_str_lit!(stringify!($expected));
            let actual_str = $crate::c_str_lit!(stringify!($actual));
            let msg_c_str = $crate::c_str_lit!($msg);
            unsafe {
                $crate::printf(
                    format,
                    file_c_str,
                    line!() as core::ffi::c_int,
                    expected_str,
                    $expected_val as isize,
                    actual_str,
                    $actual_val as isize,
                    msg_c_str,
                );
            }
            if $early_return {
                return false;
            }
        }
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! check_condition {
    ($cond:expr, $early_return:expr, $desc:literal, $actual:expr, $msg:expr) => {
        if !$cond {
            record_failure!();
            let format = $crate::failed_format!($desc);
            let file_c_str = $crate::c_str_lit!(file!());
            let actual_str = $crate::c_str_lit!(stringify!($actual));
            let msg_c_str = $crate::c_str_lit!($msg);
            unsafe {
                $crate::printf(
                    format,
                    file_c_str,
                    line!() as core::ffi::c_int,
                    actual_str,
                    msg_c_str,
                );
            }
            if $early_return {
                return false;
            }
        }
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! failed_format {
    ($body:expr) => {
        $crate::c_str_lit!(concat!(
            "\n",
            "    [FAILED]\n",
            "    %s:%d:\n",
            "    ",
            $body,
            "\n",
            "    %s\n"
        ))
    };
}

/// The data structure that statically defines a test suite, intended to be
/// defined via the #[test_suite] macro to encoded into a special section in
/// the kernel.
#[doc(hidden)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteRegistration {
    pub name: *const c_char,
    pub desc: *const c_char,
    pub tests: *const TestCaseRegistration,
    pub test_cnt: usize,
}

unsafe impl Sync for TestSuiteRegistration {}

/// The data structure defining a test case within a suite, also intended be
/// defined via the #[test_suite] macro to encoded into a special section in
/// the kernel
#[doc(hidden)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestCaseRegistration {
    pub name: *const c_char,
    pub fn_: extern "C" fn() -> bool,
}

unsafe impl Sync for TestCaseRegistration {}

/// Asserts that two expressions are equal, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_eq {
    ($expected:expr, $actual:expr) => {
        $crate::expect_eq!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e == a, false, "==", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that two expressions are equal and short-circuits on failure.
#[macro_export]
macro_rules! assert_eq {
    ($expected:expr, $actual:expr) => {
        $crate::assert_eq!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e == a, true, "==", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that two expressions are not equal, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_ne {
    ($expected:expr, $actual:expr) => {
        $crate::expect_ne!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e != a, false, "!=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that two expressions are not equal and short-circuits on failure.
#[macro_export]
macro_rules! assert_ne {
    ($expected:expr, $actual:expr) => {
        $crate::assert_ne!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e != a, true, "!=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is less than the second, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_lt {
    ($expected:expr, $actual:expr) => {
        $crate::expect_lt!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e < a, false, "<", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is less than the second and short-circuits on failure.
#[macro_export]
macro_rules! assert_lt {
    ($expected:expr, $actual:expr) => {
        $crate::assert_lt!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e < a, true, "<", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is less than or equal to the second, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_le {
    ($expected:expr, $actual:expr) => {
        $crate::expect_le!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e <= a, false, "<=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is less than or equal to the second and short-circuits on failure.
#[macro_export]
macro_rules! assert_le {
    ($expected:expr, $actual:expr) => {
        $crate::assert_le!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e <= a, true, "<=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is greater than the second, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_gt {
    ($expected:expr, $actual:expr) => {
        $crate::expect_gt!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e > a, false, ">", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is greater than the second and short-circuits on failure.
#[macro_export]
macro_rules! assert_gt {
    ($expected:expr, $actual:expr) => {
        $crate::assert_gt!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e > a, true, ">", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is greater than or equal to the second, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_ge {
    ($expected:expr, $actual:expr) => {
        $crate::expect_ge!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e >= a, false, ">=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the first expression is greater than or equal to the second and short-circuits on failure.
#[macro_export]
macro_rules! assert_ge {
    ($expected:expr, $actual:expr) => {
        $crate::assert_ge!($expected, $actual, "")
    };
    ($expected:expr, $actual:expr, $msg:expr) => {
        let e = $expected;
        let a = $actual;
        $crate::check_comparison!(e >= a, true, ">=", $expected, e, $actual, a, $msg);
    };
}

/// Asserts that the expression evaluates to true, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_true {
    ($actual:expr) => {
        $crate::expect_true!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(a, false, "%s is false", $actual, $msg);
    };
}

/// Asserts that the expression evaluates to true and short-circuits on failure.
#[macro_export]
macro_rules! assert_true {
    ($actual:expr) => {
        $crate::assert_true!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(a, true, "%s is false", $actual, $msg);
    };
}

/// Asserts that the expression evaluates to false, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_false {
    ($actual:expr) => {
        $crate::expect_false!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(!a, false, "%s is true", $actual, $msg);
    };
}

/// Asserts that the expression evaluates to false and short-circuits on failure.
#[macro_export]
macro_rules! assert_false {
    ($actual:expr) => {
        $crate::assert_false!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(!a, true, "%s is true", $actual, $msg);
    };
}

/// Asserts that the pointer is null, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_null {
    ($actual:expr) => {
        $crate::expect_null!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(a.is_null(), false, "%s is non-null!", $actual, $msg);
    };
}

/// Asserts that the pointer is null and short-circuits on failure.
#[macro_export]
macro_rules! assert_null {
    ($actual:expr) => {
        $crate::assert_null!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(a.is_null(), true, "%s is non-null!", $actual, $msg);
    };
}

/// Asserts that the pointer is non-null, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_nonnull {
    ($actual:expr) => {
        $crate::expect_nonnull!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(!a.is_null(), false, "%s is null!", $actual, $msg);
    };
}

/// Asserts that the pointer is non-null and short-circuits on failure.
#[macro_export]
macro_rules! assert_nonnull {
    ($actual:expr) => {
        $crate::assert_nonnull!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a = $actual;
        $crate::check_condition!(!a.is_null(), true, "%s is null!", $actual, $msg);
    };
}

/// Asserts that the expression evaluates to OK, but does not short-circuit on failure.
#[macro_export]
macro_rules! expect_ok {
    ($actual:expr) => {
        $crate::expect_ok!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a: ::unittest::__Status = $actual.into();
        $crate::check_comparison!(
            a == ::unittest::__Status::OK,
            false,
            "==",
            ::unittest::__Status::OK,
            ::unittest::__Status::OK.into_raw(),
            a,
            a.into_raw(),
            $msg
        );
    };
}

/// Asserts that the expression evaluates to OK and short-circuits on failure.
#[macro_export]
macro_rules! assert_ok {
    ($actual:expr) => {
        $crate::assert_ok!($actual, "")
    };
    ($actual:expr, $msg:expr) => {
        let a: ::unittest::__Status = $actual.into();
        $crate::check_comparison!(
            a == ::unittest::__Status::OK,
            true,
            "==",
            ::unittest::__Status::OK,
            ::unittest::__Status::OK.into_raw(),
            a,
            a.into_raw(),
            $msg
        );
    };
}

#[cfg(test)]
mod tests {
    use core::ffi::CStr;
    use core::{ptr, slice};
    use std::cell::Cell;
    use std::vec::Vec;

    use super::{TestSuiteRegistration, test_suite};

    unsafe extern "C" {
        static __start_unittest_testcases: TestSuiteRegistration;
        static __stop_unittest_testcases: TestSuiteRegistration;
    }

    // Thread-local since #[test] instances are run in parallel.
    thread_local! {
        static END_REACHED: Cell<bool> = Cell::new(false);
    }

    fn mark_end_as_reached() {
        END_REACHED.with(|cell| cell.set(true));
    }

    fn mark_end_as_not_reached() {
        END_REACHED.with(|cell| cell.set(false));
    }

    fn expect_end_reached() {
        std::assert_eq!(END_REACHED.with(|cell| cell.get()), true);
    }

    fn expect_end_not_reached() {
        std::assert_eq!(END_REACHED.with(|cell| cell.get()), false);
    }

    fn get_test_suites() -> Vec<TestSuiteRegistration> {
        let start = unsafe { &__start_unittest_testcases as *const TestSuiteRegistration };
        let stop = unsafe { &__stop_unittest_testcases as *const TestSuiteRegistration };

        let count = unsafe { stop.offset_from(start) } as usize;

        let test_suites_rodata = unsafe { slice::from_raw_parts(start, count) };

        let mut suites = Vec::from(test_suites_rodata);

        suites.sort_by(|a, b| {
            let a_name = unsafe { CStr::from_ptr(a.name) };
            let b_name = unsafe { CStr::from_ptr(b.name) };
            a_name.cmp(b_name)
        });
        suites
    }

    /// Suite with one function description.
    #[test_suite]
    mod suite_with_one_function {
        /// Empty function description.
        fn empty() {}
    }

    /// Assertion tests description.
    #[test_suite]
    mod assertions {
        /// Success cases.
        fn test_success() {
            assert_eq!(1, 1);
            assert_ne!(1, 2);
            assert_lt!(1, 2);
            assert_le!(1, 1);
            assert_gt!(2, 1);
            assert_ge!(2, 2);

            assert_true!(true);
            assert_false!(false);

            let null_ptr: *const i32 = ptr::null();
            let nonnull_ptr: *const i32 = &42 as *const i32;
            assert_null!(null_ptr);
            assert_nonnull!(nonnull_ptr);

            assert_ok!(zx_status::Status::OK);

            mark_end_as_reached();
        }

        /// Test that assert_eq fails on inequality.
        fn fail_assert_eq() {
            assert_eq!(1, 2);
            mark_end_as_reached();
        }

        /// Test that assert_ne fails on equality.
        fn fail_assert_ne() {
            assert_ne!(1, 1);
            mark_end_as_reached();
        }

        /// Test that assert_lt fails when not less-than.
        fn fail_assert_lt() {
            assert_lt!(2, 1);
            mark_end_as_reached();
        }

        /// Test that assert_le fails when greater.
        fn fail_assert_le() {
            assert_le!(2, 1);
            mark_end_as_reached();
        }

        /// Test that assert_gt fails when not greater-than.
        fn fail_assert_gt() {
            assert_gt!(1, 2);
            mark_end_as_reached();
        }

        /// Test that assert_ge fails when less.
        fn fail_assert_ge() {
            assert_ge!(1, 2);
            mark_end_as_reached();
        }

        /// Test that assert_true fails when value is false.
        fn fail_assert_true() {
            assert_true!(false);
            mark_end_as_reached();
        }

        /// Test that assert_false fails when value is true.
        fn fail_assert_false() {
            assert_false!(true);
            mark_end_as_reached();
        }

        /// Test that assert_null fails when pointer is non-null.
        fn fail_assert_null() {
            assert_null!(&42 as *const i32);
            mark_end_as_reached();
        }

        /// Test that assert_nonnull fails when pointer is null.
        fn fail_assert_nonnull() {
            assert_nonnull!(ptr::null::<i32>());
            mark_end_as_reached();
        }

        /// Test that assert_ok fails when value is non-zero.
        fn fail_assert_ok() {
            assert_ok!(zx_status::Status::INTERNAL);
            mark_end_as_reached();
        }
    }

    /// Expectation tests description.
    #[test_suite]
    mod expectations {
        /// Success cases.
        fn test_success() {
            expect_eq!(1, 1);
            expect_eq!(1, 1, "one should be one");
            expect_ne!(1, 2);
            expect_ne!(1, 2, "one should not be two");
            expect_lt!(1, 2);
            expect_lt!(1, 2, "one should be less than two");
            expect_le!(1, 1);
            expect_le!(1, 2);
            expect_gt!(2, 1);
            expect_ge!(2, 2);

            expect_true!(true);
            expect_true!(true, "should be true");
            expect_false!(false);
            expect_false!(false, "should be false");

            let null_ptr: *const i32 = ptr::null();
            let nonnull_ptr: *const i32 = &42 as *const i32;
            expect_null!(null_ptr);
            expect_null!(null_ptr, "should be null");
            expect_nonnull!(nonnull_ptr);
            expect_nonnull!(nonnull_ptr, "should be non-null");

            expect_ok!(zx_status::Status::OK);
            expect_ok!(zx_status::Status::OK, "should be OK");

            mark_end_as_reached();
        }

        /// Test that expect_eq fails on inequality.
        fn fail_expect_eq() {
            expect_eq!(1, 2);
            mark_end_as_reached();
        }

        /// Test that expect_ne fails on equality.
        fn fail_expect_ne() {
            expect_ne!(1, 1);
            mark_end_as_reached();
        }

        /// Test that expect_lt fails when not less-than.
        fn fail_expect_lt() {
            expect_lt!(2, 1);
            mark_end_as_reached();
        }

        /// Test that expect_le fails when greater.
        fn fail_expect_le() {
            expect_le!(2, 1);
            mark_end_as_reached();
        }

        /// Test that expect_gt fails when not greater-than.
        fn fail_expect_gt() {
            expect_gt!(1, 2);
            mark_end_as_reached();
        }

        /// Test that expect_ge fails when less.
        fn fail_expect_ge() {
            expect_ge!(1, 2);
            mark_end_as_reached();
        }

        /// Test that expect_true fails when value is false.
        fn fail_expect_true() {
            expect_true!(false);
            mark_end_as_reached();
        }

        /// Test that expect_false fails when value is true.
        fn fail_expect_false() {
            expect_false!(true);
            mark_end_as_reached();
        }

        /// Test that expect_null fails when pointer is non-null.
        fn fail_expect_null() {
            expect_null!(&42 as *const i32);
            mark_end_as_reached();
        }

        /// Test that expect_nonnull fails when pointer is null.
        fn fail_expect_nonnull() {
            expect_nonnull!(ptr::null::<i32>());
            mark_end_as_reached();
        }

        /// Test that expect_ok fails when value is non-zero.
        fn fail_expect_ok() {
            expect_ok!(zx_status::Status::INTERNAL);
            mark_end_as_reached();
        }
    }

    #[test]
    fn check_suite_count() {
        let suites = get_test_suites();
        std::assert_eq!(suites.len(), 3);
    }

    #[test]
    fn check_suite_assertions() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 0);
        let suite = &suites[0];

        std::assert_eq!(unsafe { CStr::from_ptr(suite.name) }.to_bytes(), b"assertions");
        std::assert_eq!(suite.test_cnt, 12);

        let cases_rodata = unsafe { slice::from_raw_parts(suite.tests, suite.test_cnt) };
        for case in cases_rodata {
            mark_end_as_not_reached();
            let name = unsafe { CStr::from_ptr(case.name) }.to_str().unwrap();
            let res = (case.fn_)();
            if name == "test_success" {
                std::assert_eq!(res, true, "assertions::test_success should pass");
                expect_end_reached();
            } else {
                std::assert_eq!(res, false, "assertions::{} should fail", name);
                expect_end_not_reached();
            }
        }
    }

    #[test]
    fn check_suite_expectations() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 1);
        let suite = &suites[1];

        std::assert_eq!(unsafe { CStr::from_ptr(suite.name) }.to_bytes(), b"expectations");
        std::assert_eq!(suite.test_cnt, 12);

        let cases = unsafe { slice::from_raw_parts(suite.tests, suite.test_cnt) };
        for case in cases {
            mark_end_as_not_reached();
            let name = unsafe { CStr::from_ptr(case.name) }.to_str().unwrap();
            let res = (case.fn_)();
            if name == "test_success" {
                std::assert_eq!(res, true, "expectations::test_success should pass");
            } else {
                std::assert_eq!(res, false, "expectations::{} should fail", name);
            }
            expect_end_reached();
        }
    }

    #[test]
    fn check_suite_with_one_function() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 2);
        let suite = &suites[2];

        std::assert_eq!(
            unsafe { CStr::from_ptr(suite.name) }.to_bytes(),
            b"suite_with_one_function"
        );
        std::assert_eq!(
            unsafe { CStr::from_ptr(suite.desc) }.to_str().unwrap(),
            "Suite with one function description."
        );
        std::assert_eq!(suite.test_cnt, 1);
        let case = unsafe { &*suite.tests };
        std::assert_eq!(unsafe { CStr::from_ptr(case.name) }.to_bytes(), b"empty");
        assert!((case.fn_)());
    }
}
