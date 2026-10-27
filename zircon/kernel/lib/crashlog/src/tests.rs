// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// crashlog tests
#[unittest::suite(name = "crashlog")]
mod crashlog_tests {
    use kprint::kprintln;
    use unittest::{
        assert_eq, assert_gt, assert_le, assert_ok, assert_true, expect_eq, expect_false,
        expect_gt, expect_lt, expect_true, subtest, unwrap_some,
    };
    use zx_status::Status;

    use super::super::crashlog_to_string;
    use super::super::panic_buffer::PanicBuffer;
    use crate::platform_rs::debug::{platform_serial_resume, platform_serial_suspend};
    use crate::platform_rs::power::{ZirconCrashReason, platform_serial_is_suspended};

    const LARGE_BUFFER_SIZE: usize = 8 * 1024;
    const TOO_SMALL_BUFFER_SIZE: usize = 64;

    fn contains(text: &[u8], needle: &[u8]) -> bool {
        text.windows(needle.len()).any(|window| window == needle)
    }

    fn expect_banner(text: &[u8]) -> bool {
        let check = subtest!(|text: &[u8]| {
            expect_true!(contains(text, b"ZIRCON REBOOT REASON"));
            expect_true!(contains(text, b"UPTIME"));
            expect_true!(contains(text, b"build ID"));
            expect_true!(contains(text, b"zx_system_get_version_string:"));
        });
        check(text)
    }

    /// NoCrash
    #[test]
    fn no_crash_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // NoCrash should currently produce no crash payload at all.
        let len = crashlog_to_string(&mut buffer, ZirconCrashReason::NoCrash as u32);
        expect_eq!(0, len);
    }

    /// Panic/SW WDT
    #[test]
    fn panic_wdt_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // Panic and SoftwareWatchdog reasons should produce all of the sections.
        const REASONS: [ZirconCrashReason; 2] =
            [ZirconCrashReason::Panic, ZirconCrashReason::SoftwareWatchdog];

        for reason in REASONS {
            buffer.fill(0);
            let len = crashlog_to_string(&mut buffer, reason as u32);
            assert_le!(len, LARGE_BUFFER_SIZE);
            assert_gt!(len, 0);

            let text = &buffer[..len];

            // Banner.
            expect_true!(expect_banner(text));

            // Debug info.
            expect_true!(contains(text, b"{{{reset}}}"));
            expect_true!(contains(text, b"REGISTERS"));
            expect_true!(contains(text, b"BACKTRACE"));

            // Critical counters.
            expect_true!(contains(text, b"counters: "));

            // Panic buffer.
            expect_true!(contains(text, b"panic buffer: "));

            // Debug log.
            expect_true!(contains(text, b"--- BEGIN DLOG DUMP ---"));
            expect_true!(contains(text, b"--- END DLOG DUMP ---"));
        }
    }

    /// OOM/RootJob
    #[test]
    fn oom_root_job_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // OOM and UserspaceRootJobTermination reasons should produce all of the
        // sections, except for the debugging context and panic buffer.
        const REASONS: [ZirconCrashReason; 2] =
            [ZirconCrashReason::Oom, ZirconCrashReason::UserspaceRootJobTermination];

        for reason in REASONS {
            buffer.fill(0);
            let len = crashlog_to_string(&mut buffer, reason as u32);
            assert_le!(len, LARGE_BUFFER_SIZE);
            assert_gt!(len, 0);

            let text = &buffer[..len];

            // Banner.
            expect_true!(expect_banner(text));

            // No debug info.
            expect_false!(contains(text, b"{{{reset}}}"));
            expect_false!(contains(text, b"REGISTERS"));
            expect_false!(contains(text, b"BACKTRACE"));

            // Critical counters.
            expect_true!(contains(text, b"counters: "));

            // No panic buffer.
            expect_false!(contains(text, b"panic buffer: "));

            // Debug log.
            expect_true!(contains(text, b"--- BEGIN DLOG DUMP ---"));
            expect_true!(contains(text, b"--- END DLOG DUMP ---"));
        }
    }

    /// UnknownReason
    #[test]
    fn unknown_test() {
        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // Unknown reasons should include just the banner.
        const REASONS: [u32; 2] = [ZirconCrashReason::Unknown as u32, 0xbaadf00d];

        for reason in REASONS {
            buffer.fill(0);
            let len = crashlog_to_string(&mut buffer, reason);
            assert_le!(len, LARGE_BUFFER_SIZE);
            assert_gt!(len, 0);

            let text = &buffer[..len];

            // Banner.
            expect_true!(expect_banner(text));

            // No debug info.
            expect_false!(contains(text, b"{{{reset}}}"));
            expect_false!(contains(text, b"REGISTERS"));
            expect_false!(contains(text, b"BACKTRACE"));

            // No critical counters.
            expect_false!(contains(text, b"counters: "));

            // No panic buffer.
            expect_false!(contains(text, b"panic buffer: "));

            // No debug log.
            expect_false!(contains(text, b"--- BEGIN DLOG DUMP ---"));
            expect_false!(contains(text, b"--- END DLOG DUMP ---"));
        }
    }

    /// Truncation
    #[test]
    fn truncation_test() {
        const CANARY_VALUE: u8 = 0xa5;
        let mut buffer = [0u8; TOO_SMALL_BUFFER_SIZE + 1];

        buffer[TOO_SMALL_BUFFER_SIZE] = CANARY_VALUE;

        let len = crashlog_to_string(
            &mut buffer[..TOO_SMALL_BUFFER_SIZE],
            ZirconCrashReason::Panic as u32,
        );
        expect_lt!(len, buffer.len());
        expect_gt!(len, 0);

        expect_eq!(buffer[TOO_SMALL_BUFFER_SIZE], CANARY_VALUE);
    }

    /// PanicBuffer
    #[test]
    fn panic_buffer_test() {
        let panic_buffer = PanicBuffer::new();
        assert_eq!(0, panic_buffer.with_contents(|contents| contents.len()));

        const MSG: &[u8] = b"hello";
        let msg_len = MSG.len();
        panic_buffer.append(MSG);
        assert_true!(panic_buffer.with_contents(|contents| contents == MSG));
        assert_eq!(msg_len, panic_buffer.with_contents(|contents| contents.len()));

        // Grossly over append.
        for i in 0..PanicBuffer::MAX_SIZE {
            panic_buffer.append(MSG);
            let expected_size = ((i + 2) * msg_len).min(PanicBuffer::MAX_SIZE - 1);
            assert_eq!(expected_size, panic_buffer.with_contents(|contents| contents.len()));
        }
        assert_eq!(
            PanicBuffer::MAX_SIZE - 1,
            panic_buffer.with_contents(|contents| contents.len())
        );
    }

    // Verify that the rendered crashlog includes a note indicating the debug UART
    // was suspended if (and only if) the crash happened while the UART was
    // suspended via `platform_serial_suspend`.  Without this note, a reader of the
    // crashlog would have no explanation for the missing serial console output.
    /// SerialSuspended
    #[test]
    fn serial_suspended_test() {
        const NOTE: &[u8] = b"DEBUG UART SUSPENDED";

        let mut buffer =
            unwrap_some!(kalloc::Box::<[u8]>::try_new_zeroed_slice(LARGE_BUFFER_SIZE).ok());

        // Not suspended, so there should be no note.
        let len = crashlog_to_string(&mut buffer, ZirconCrashReason::Panic as u32);
        assert_le!(len, LARGE_BUFFER_SIZE);
        expect_false!(contains(&buffer[..len], NOTE));

        let status = Status::ok(platform_serial_suspend());
        if status == Err(Status::NOT_SUPPORTED) {
            kprintln!("debug UART suspend not supported, skipping");
            return true;
        }
        assert_ok!(status);
        expect_true!(platform_serial_is_suspended());

        buffer.fill(0);
        let len = crashlog_to_string(&mut buffer, ZirconCrashReason::Panic as u32);

        assert_ok!(Status::ok(platform_serial_resume()));
        expect_false!(platform_serial_is_suspended());

        assert_le!(len, LARGE_BUFFER_SIZE);
        let text = &buffer[..len];
        expect_true!(expect_banner(text));
        expect_true!(contains(text, NOTE));
    }
}
