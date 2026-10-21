// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic lamp log test suite for babeldiff's tests, trimmed from the
// kernel's crashlog tests. It is not real kernel code.

/// lamplog tests
#[unittest::suite(name = "lamplog")]
mod lamplog_tests {
    use unittest::{assert_gt, assert_le, expect_eq, expect_gt, expect_true, unwrap_some};

    use super::super::lamplog_to_string;
    use crate::lamp::LampOffReason;

    const LARGE_BUFFER_SIZE: usize = 8 * 1024;

    /// Returns whether `needle` occurs in `text`.
    fn contains(text: &[u8], needle: &[u8]) -> bool {
        text.windows(needle.len()).any(|window| window == needle)
    }

    /// NoGlow
    #[test]
    fn no_glow_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // NoGlow should currently produce no log payload at all.
        let len = lamplog_to_string(&mut buffer, LampOffReason::NoGlow as u32);
        expect_eq!(0, len);
    }

    /// Flicker
    #[test]
    fn flicker_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // Flicker and Burnout reasons should produce all of the sections.
        const REASONS: [LampOffReason; 2] = [LampOffReason::Flicker, LampOffReason::Burnout];

        for reason in REASONS {
            buffer.fill(0);
            let len = lamplog_to_string(&mut buffer, reason as u32);
            assert_le!(len, LARGE_BUFFER_SIZE);
            assert_gt!(len, 0);

            let text = &buffer[..len];

            // Wick info.
            expect_true!(contains(text, b"WICK"));

            // Glow log.
            expect_true!(contains(text, b"--- BEGIN GLOW DUMP ---"));
            expect_true!(contains(text, b"--- END GLOW DUMP ---"));
        }
    }

    /// Dim
    #[test]
    fn dim_test() {
        let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE) else {
            return;
        };

        // Dim should produce just the banner.
        let len = lamplog_to_string(&mut buffer, LampOffReason::Dim as u32);
        expect_gt!(len, 0);
    }
}
