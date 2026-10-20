// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel unit tests for the Rust `libc::stdio::StringFile`.

/// StringFile tests
#[unittest::suite(name = "string_file_rust")]
mod string_file_tests {
    use core::fmt::Write as _;

    use libc::stdio::StringFile;
    use unittest::{assert_eq, assert_true, expect_eq, expect_true, subtest};

    const FILL_BYTE: u8 = 0xff;

    /// Checks the used and available regions of `file`, whose buffer starts at address `base` and
    /// holds `capacity` bytes, after `written` bytes have been written to it.
    fn validate_regions(
        file: &mut StringFile<'_>,
        base: usize,
        capacity: usize,
        written: usize,
    ) -> bool {
        let check =
            subtest!(|file: &mut StringFile<'_>, base: usize, capacity: usize, written: usize| {
                let effective_capacity = capacity.saturating_sub(1);
                let expected_used = written.min(effective_capacity);
                let expected_avail = effective_capacity - expected_used;

                let used_region = file.used_region();
                assert_eq!(expected_used, used_region.len());
                // Unlike C++ spans, empty Rust slices have no meaningful (null) base address, so
                // the base is only checked for non-empty regions.
                if expected_used > 0 {
                    assert_eq!(base, used_region.as_ptr() as usize);
                }

                let available_region = file.available_region();
                assert_eq!(expected_avail, available_region.len());
                if expected_avail > 0 {
                    assert_eq!(base + expected_used, available_region.as_ptr() as usize);
                }
            });
        check(file, base, capacity, written)
    }

    /// Checks that the contents of `file` match `expected`.
    fn validate_contents(file: &StringFile<'_>, expected: &[u8]) -> bool {
        let check = subtest!(|file: &StringFile<'_>, expected: &[u8]| {
            // Test the `as_bytes` method.
            let contents = file.as_bytes();
            assert_eq!(expected.len(), contents.len());
            assert_true!(contents == expected);

            // Same test, but using the AsRef conversion instead.
            let contents: &[u8] = file.as_ref();
            assert_eq!(expected.len(), contents.len());
            assert_true!(contents == expected);
        });
        check(file, expected)
    }

    /// StringFile::Write - With Empty Buffer
    #[test]
    fn write_on_empty_buffer_is_ok() {
        const INPUT: &[u8] = b"12345";

        let mut string_file = StringFile::new(&mut []);
        expect_eq!(string_file.write(INPUT), INPUT.len());
    }

    /// StringFile::Write - With Empty Input
    #[test]
    fn write_empty_string_view_is_ok() {
        let mut buffer = [0u8; 1];

        let mut string_file = StringFile::new(&mut buffer);

        expect_eq!(string_file.write(&[]), 0);

        let written_view = string_file.take();
        assert_true!(!written_view.is_empty());
        assert_eq!(buffer[0], 0);
    }

    /// StringFile::Write - Input fits in buffer
    #[test]
    fn write_string_view_that_fits_in_buffer() {
        const INPUT: &[u8] = b"12345";
        let mut buffer = [0u8; INPUT.len() + 1];
        let base = buffer.as_ptr() as usize;

        let mut string_file = StringFile::new(&mut buffer);

        expect_eq!(string_file.write(INPUT), INPUT.len());

        let written_view = string_file.take();

        expect_true!(&written_view[..INPUT.len()] == INPUT);
        assert_eq!(written_view.as_ptr() as usize, base);
    }

    /// StringFile::Write - Input does not fit in buffer
    #[test]
    fn write_truncate_string_view_that_doesnt_fits_in_buffer() {
        const INPUT: &[u8] = b"12345";
        // We would reserve last char for '\0'
        let mut buffer = [0u8; INPUT.len()];
        let base = buffer.as_ptr() as usize;

        let mut string_file = StringFile::new(&mut buffer);

        expect_eq!(string_file.write(INPUT), INPUT.len());

        let written_view = string_file.take();

        assert_eq!(written_view.len(), INPUT.len());
        let written_len = written_view.len();
        expect_true!(written_view[..written_len - 1] == INPUT[..written_len - 1]);
        assert_eq!(written_view.as_ptr() as usize, base);
        assert_eq!(buffer[buffer.len() - 1], 0);
    }

    /// StringFile::Write - Multiple Calls are correct
    #[test]
    fn write_many_times() {
        const INPUT: &[u8] = b"12345";
        // We would reserve last char for '\0'
        let mut buffer = [FILL_BYTE; INPUT.len()];
        let base = buffer.as_ptr() as usize;

        let mut string_file = StringFile::new(&mut buffer);

        expect_eq!(string_file.write(&INPUT[0..2]), 2);
        expect_true!(string_file.used_region()[0..2] == INPUT[0..2]);

        expect_eq!(string_file.write(&INPUT[2..3]), 1);
        expect_true!(string_file.used_region()[2..3] == INPUT[2..3]);

        expect_eq!(string_file.write(&INPUT[3..5]), 2);
        expect_true!(string_file.used_region()[3..4] == INPUT[3..4]);

        // The buffer cannot be read while `string_file` borrows it, so instead of checking that the
        // last character is still untouched, check that only the space before it has been used.
        assert_eq!(string_file.used_region().len(), INPUT.len() - 1);
        assert_eq!(string_file.available_region().len(), 0);

        let written_view = string_file.take();

        assert_eq!(written_view.len(), INPUT.len());
        let written_len = written_view.len();
        expect_true!(written_view[..written_len - 1] == INPUT[..written_len - 1]);
        assert_eq!(written_view.as_ptr() as usize, base);
        assert_eq!(buffer[buffer.len() - 1], 0);
    }

    /// StringFile::take - Adds Null Character
    #[test]
    fn take_adds_null_character() {
        const INPUT: &[u8] = b"12345";
        // We would reserve last char for '\0'
        let mut buffer = [FILL_BYTE; INPUT.len()];
        let base = buffer.as_ptr() as usize;

        // The buffer cannot be read while a `StringFile` borrows it, so write once and drop the
        // file to inspect the buffer, then repeat the same write with a fresh file to `take()`.
        {
            let mut string_file = StringFile::new(&mut buffer);
            expect_eq!(string_file.write(INPUT), INPUT.len());
        }
        // Write should not have set the last character set yet.
        assert_eq!(buffer[buffer.len() - 1], FILL_BYTE);

        let mut string_file = StringFile::new(&mut buffer);
        expect_eq!(string_file.write(INPUT), INPUT.len());
        let written_view = string_file.take();
        let written_len = written_view.len();
        let written_ptr = written_view.as_ptr() as usize;
        let prefix_matches = written_view[..written_len - 1] == INPUT[..written_len - 1];
        assert_eq!(buffer[buffer.len() - 1], 0);

        assert_eq!(written_len, INPUT.len());
        expect_true!(prefix_matches);
        assert_eq!(written_ptr, base);
    }

    /// StringFile::take - Empty Buffer
    #[test]
    fn take_on_empty_buffer_is_empty() {
        let string_file = StringFile::new(&mut []);
        let written_view = string_file.take();
        assert_true!(written_view.is_empty());
    }

    /// StringFile avail/used space
    #[test]
    fn available_used_space() {
        // Empty files should always report no space remaining, and no space used.
        let mut empty_file = StringFile::new(&mut []);
        assert_true!(validate_regions(&mut empty_file, 0, 0, 0));

        // Writing to the file with no buffer should not change anything.
        empty_file.write(b"x");
        assert_true!(validate_regions(&mut empty_file, 0, 0, 1));

        // Repeat the tests, but now with a file backed by a non-empty buffer.
        let mut buffer = [0u8; 4];
        let capacity = buffer.len();
        let base = buffer.as_ptr() as usize;
        let mut string_file = StringFile::new(&mut buffer);
        for i in 0..capacity {
            assert_true!(validate_regions(&mut string_file, base, capacity, i));
            string_file.write(b"x");
            assert_true!(validate_regions(&mut string_file, base, capacity, i + 1));
        }
    }

    /// StringFile::Skip
    #[test]
    fn skip() {
        let mut buffer = [0u8; 10];

        // Skip a part of the start of the file, but overwrite the end.
        {
            buffer.fill(b'x');
            let mut string_file = StringFile::new(&mut buffer);

            string_file.skip(3);
            string_file.write(b"123456789abcde");

            const EXPECTED_STR: &[u8] = b"xxx123456\0";
            let actual_str = string_file.take();
            assert_eq!(EXPECTED_STR.len(), actual_str.len());
            assert_true!(actual_str == EXPECTED_STR);
        }

        // Skip some of the middle of a file.
        {
            buffer.fill(b'x');
            let mut string_file = StringFile::new(&mut buffer);

            string_file.write(b"123");
            string_file.skip(3);
            string_file.write(b"456789abcde");

            const EXPECTED_STR: &[u8] = b"123xxx456\0";
            let actual_str = string_file.take();
            assert_eq!(EXPECTED_STR.len(), actual_str.len());
            assert_true!(actual_str == EXPECTED_STR);
        }

        // Attempt to skip past the end of a file.
        {
            buffer.fill(b'x');
            let mut string_file = StringFile::new(&mut buffer);

            string_file.write(b"123456");
            string_file.skip(30);
            string_file.write(b"789abcde");

            const EXPECTED_STR: &[u8] = b"123456xxx\0";
            let actual_str = string_file.take();
            assert_eq!(EXPECTED_STR.len(), actual_str.len());
            assert_true!(actual_str == EXPECTED_STR);
        }
    }

    /// StringFile string_view conversion
    #[test]
    fn string_view_conversion() {
        {
            // A file with no buffer should always yield an empty string view.
            let mut empty_file = StringFile::new(&mut []);
            assert_true!(validate_contents(&empty_file, b""));

            // Attempting to write to the file should not change this.
            empty_file.write(b"12345");
            assert_true!(validate_contents(&empty_file, b""));
        }

        {
            const PATTERN: &[u8] = b"1234";
            let mut buffer = [0u8; 10];
            let mut string_file = StringFile::new(&mut buffer);

            assert_true!(validate_contents(&string_file, b""));
            string_file.write(PATTERN);
            assert_true!(validate_contents(&string_file, b"1234"));
            string_file.write(PATTERN);
            assert_true!(validate_contents(&string_file, b"12341234"));
            string_file.write(PATTERN);
            assert_true!(validate_contents(&string_file, b"123412341"));
            string_file.write(PATTERN);
            assert_true!(validate_contents(&string_file, b"123412341"));
        }
    }

    /// StringFile::with_file - Writes through ScopedFile append and truncate
    #[test]
    fn with_file_round_trip() {
        let mut buffer = [0u8; 16];
        let mut string_file = StringFile::new(&mut buffer);

        string_file.write(b"ab");
        let (empty, raw, formatted) = string_file.with_file(|file| {
            // Writes through the `FILE` land in the `StringFile` after what is already there.
            let empty = file.write(b"");
            let raw = file.write(b"cd");
            let formatted = write!(file, "{}", 42).is_ok();
            (empty, raw, formatted)
        });
        expect_eq!(empty, 0);
        expect_eq!(raw, 2);
        expect_true!(formatted);
        string_file.write(b"ef");
        assert_true!(validate_contents(&string_file, b"abcd42ef"));

        // Writes through the `FILE` are truncated just like direct writes.
        string_file.with_file(|file| {
            file.write(b"0123456789");
        });
        assert_true!(validate_contents(&string_file, b"abcd42ef0123456"));

        let taken = string_file.take();
        assert_true!(&taken[..] == b"abcd42ef0123456\0");
    }
}
