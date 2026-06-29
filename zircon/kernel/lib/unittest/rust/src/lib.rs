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

// TODO(https://fxbug.dev/517130174): assert_*/expect_* declarative macros.

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
///     fn my_case() {}
/// }
/// ```
///
pub use unittest_macro::test_suite;

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

#[cfg(test)]
mod tests {
    use core::ffi::CStr;
    use core::slice;
    use std::vec::Vec;

    use super::TestSuiteRegistration;
    use crate::test_suite;

    unsafe extern "C" {
        static __start_unittest_testcases: TestSuiteRegistration;
        static __stop_unittest_testcases: TestSuiteRegistration;
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

    /// First suite description.
    #[test_suite]
    mod suite_a {
        /// First test case description.
        fn test_case_one() {
            println!("test case #1!");
        }
    }

    /// Second suite description.
    #[test_suite]
    mod suite_b {
        /// Second test case description.
        fn test_case_two() {
            println!("test case #2!");
        }

        /// Third test case description.
        fn test_case_three() {
            println!("test case #3!");
        }
    }

    /// Third suite description.
    #[test_suite]
    mod suite_c {
        /// Fourth test case description.
        fn test_case_four() {
            println!("test case #4!");
        }
    }

    #[test]
    fn check_suite_count() {
        let suites = get_test_suites();
        std::assert_eq!(suites.len(), 3);
    }

    #[test]
    fn check_suite_a() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 0);
        let suite = &suites[0];

        std::assert_eq!(unsafe { CStr::from_ptr(suite.name) }.to_bytes(), b"suite_a");
        std::assert_eq!(
            unsafe { CStr::from_ptr(suite.desc) }.to_str().unwrap(),
            "First suite description."
        );
        std::assert_eq!(suite.test_cnt, 1);
        let case = unsafe { &*suite.tests };
        std::assert_eq!(unsafe { CStr::from_ptr(case.name) }.to_bytes(), b"test_case_one");
        assert!((case.fn_)());
    }

    #[test]
    fn check_suite_b() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 1);
        let suite = &suites[1];

        std::assert_eq!(unsafe { CStr::from_ptr(suite.name) }.to_bytes(), b"suite_b");
        std::assert_eq!(
            unsafe { CStr::from_ptr(suite.desc) }.to_str().unwrap(),
            "Second suite description."
        );
        std::assert_eq!(suite.test_cnt, 2);

        let cases_rodata = unsafe { slice::from_raw_parts(suite.tests, suite.test_cnt) };
        let mut cases = Vec::from(cases_rodata);

        cases.sort_by(|a, b| {
            let a_name = unsafe { CStr::from_ptr(a.name) };
            let b_name = unsafe { CStr::from_ptr(b.name) };
            a_name.cmp(b_name)
        });

        std::assert_eq!(unsafe { CStr::from_ptr(cases[0].name) }.to_bytes(), b"test_case_three");
        assert!((cases[0].fn_)());

        std::assert_eq!(unsafe { CStr::from_ptr(cases[1].name) }.to_bytes(), b"test_case_two");
        assert!((cases[1].fn_)());
    }

    #[test]
    fn check_suite_c() {
        let suites = get_test_suites();
        std::assert!(suites.len() > 2);
        let suite = &suites[2];

        std::assert_eq!(unsafe { CStr::from_ptr(suite.name) }.to_bytes(), b"suite_c");
        std::assert_eq!(
            unsafe { CStr::from_ptr(suite.desc) }.to_str().unwrap(),
            "Third suite description."
        );
        std::assert_eq!(suite.test_cnt, 1);
        let case = unsafe { &*suite.tests };
        std::assert_eq!(unsafe { CStr::from_ptr(case.name) }.to_bytes(), b"test_case_four");
        assert!((case.fn_)());
    }
}
