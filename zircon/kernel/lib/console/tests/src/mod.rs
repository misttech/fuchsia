// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Tests for the kernel console.
#[cfg(all(console_enabled, ktest))]
#[unittest::suite(name = "console_rust")]
mod console_tests {

    use crate::console_rust::console::{
        CMD_AVAIL_ALWAYS, CMD_AVAIL_NORMAL, CMD_FLAG_PANIC, Cmd, CmdArgs, static_command,
    };
    use core::ffi::c_int;
    use core::sync::atomic::{AtomicI32, Ordering};
    use unittest::{assert_nonnull, expect_eq, expect_false, expect_lt, expect_ne, expect_true};
    use zx_status::Status;

    macro_rules! zx_status {
        ($status:expr) => {
            $status.into_raw()
        };
    }

    // FFI imports.
    unsafe extern "C" {
        static __start_commands: Cmd;
        static __stop_commands: Cmd;
        fn rust_console_match_command(name: *const core::ffi::c_char, mask: u8) -> *const Cmd;
        fn rust_console_get_echo() -> bool;
        fn rust_console_get_exit() -> bool;
        fn rust_console_set_exit(val: bool);
        fn console_run_script_locked(string: *const core::ffi::c_char) -> c_int;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(clippy::upper_case_acronyms)]
    struct FILE {
        write: unsafe extern "C" fn(
            *mut core::ffi::c_void,
            *const core::ffi::c_char,
            usize,
        ) -> core::ffi::c_int,
        ptr: *mut core::ffi::c_void,
    }

    unsafe extern "C" {
        static mut gStdout: FILE;
    }

    struct CapturerState {
        buf: *mut u8,
        buf_size: usize,
        len: usize,
        original_stdout: Option<FILE>,
    }

    struct SyncUnsafeCell<T>(core::cell::UnsafeCell<T>);

    // SAFETY: This is safe because we only run tests sequentially in the kernel console test thread.
    unsafe impl<T> Sync for SyncUnsafeCell<T> {}

    static CAPTURER_STATE: SyncUnsafeCell<CapturerState> =
        SyncUnsafeCell(core::cell::UnsafeCell::new(CapturerState {
            buf: core::ptr::null_mut(),
            buf_size: 0,
            len: 0,
            original_stdout: None,
        }));

    unsafe extern "C" fn write_callback(
        _ptr: *mut core::ffi::c_void,
        str_ptr: *const core::ffi::c_char,
        len: usize,
    ) -> core::ffi::c_int {
        let state = unsafe { &mut *CAPTURER_STATE.0.get() };
        if state.buf.is_null() || state.buf_size == 0 {
            return 0;
        }
        let to_copy = core::cmp::min(len, state.buf_size - 1 - state.len);
        if to_copy > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    str_ptr as *const u8,
                    state.buf.add(state.len),
                    to_copy,
                );
            }
            state.len += to_copy;
            unsafe { *state.buf.add(state.len) = 0 };
        }
        len as core::ffi::c_int
    }

    unsafe fn test_capture_stdout_start(buf: *mut u8, size: usize) {
        let state = unsafe { &mut *CAPTURER_STATE.0.get() };
        state.buf = buf;
        state.buf_size = size;
        state.len = 0;
        unsafe { *state.buf = 0 };
        unsafe {
            state.original_stdout = Some(gStdout);
            gStdout = FILE { write: write_callback, ptr: core::ptr::null_mut() };
        }
    }

    unsafe fn test_capture_stdout_stop() {
        let state = unsafe { &mut *CAPTURER_STATE.0.get() };
        if let Some(orig) = state.original_stdout {
            unsafe {
                gStdout = orig;
            }
            state.original_stdout = None;
        }
        state.buf = core::ptr::null_mut();
        state.buf_size = 0;
        state.len = 0;
    }

    // Statically registered commands.
    static_command!(
        TEST_CMD,
        c"mock_success".as_ptr(),
        c"mock_success help".as_ptr(),
        mock_success_callback,
        CMD_AVAIL_NORMAL
    );

    static_command!(
        MOCK_FAILURE,
        c"mock_failure".as_ptr(),
        core::ptr::null(),
        mock_failure_callback,
        CMD_AVAIL_NORMAL
    );

    static_command!(
        CMD_MOCK_ALWAYS,
        c"mock_avail_always".as_ptr(),
        c"mock_avail_always help".as_ptr(),
        mock_success_callback,
        CMD_AVAIL_ALWAYS
    );

    static MOCK_CALL_COUNT: AtomicI32 = AtomicI32::new(0);

    unsafe extern "C" fn mock_success_callback(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        MOCK_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
        zx_status!(Status::OK)
    }

    unsafe extern "C" fn mock_failure_callback(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        MOCK_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
        zx_status!(Status::INVALID_ARGS)
    }

    fn contains_command_help(output: &[u8], name: &[u8], help: &[u8]) -> bool {
        output.split(|&b| b == b'\n').any(|line| {
            line.starts_with(b"\t") && line[1..].starts_with(name) && line.ends_with(help)
        })
    }

    fn find_command_index(output: &[u8], name: &[u8]) -> Option<usize> {
        for (idx, line) in output.split(|&b| b == b'\n').enumerate() {
            if line.starts_with(b"\t") && line[1..].starts_with(name) {
                return Some(idx);
            }
        }
        None
    }

    /// Verify size and alignment compatibility with C++ structs.
    #[test]
    fn console_abi_test() {
        assert_eq!(core::mem::size_of::<CmdArgs>(), 40);
        assert_eq!(core::mem::align_of::<CmdArgs>(), 8);
        assert_eq!(core::mem::size_of::<Cmd>(), 32);
        assert_eq!(core::mem::align_of::<Cmd>(), 8);
    }

    /// Test echo command callback in Rust
    #[test]
    fn command_echo_test() {
        let original_echo = unsafe { rust_console_get_echo() };

        // Set echo setting to false.
        let res = unsafe { console_run_script_locked(c"echo false".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));
        expect_false!(unsafe { rust_console_get_echo() });

        // Set echo setting to true.
        let res = unsafe { console_run_script_locked(c"echo true".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));
        expect_true!(unsafe { rust_console_get_echo() });

        // Restore original.
        let cmd = if original_echo { c"echo true" } else { c"echo false" };
        unsafe {
            console_run_script_locked(cmd.as_ptr());
        }
    }

    /// Test exit command callback in Rust
    #[test]
    fn command_exit_test() {
        let original_exit = unsafe { rust_console_get_exit() };

        unsafe {
            rust_console_set_exit(false);
        }
        let res = unsafe { console_run_script_locked(c"exit".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));
        expect_false!(unsafe { rust_console_get_exit() });

        // Restore.
        unsafe {
            rust_console_set_exit(original_exit);
        }
    }

    /// Test boot-test-success command callback in Rust
    #[test]
    fn command_boot_test_success_test() {
        // Test success.
        unsafe {
            console_run_script_locked(c"mock_success".as_ptr());
        }
        let res = unsafe { console_run_script_locked(c"boot-test-success".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));

        // Test failure.
        let some_failure = unsafe { console_run_script_locked(c"mock_failure".as_ptr()) };
        let res = unsafe { console_run_script_locked(c"boot-test-success".as_ptr()) };
        expect_eq!(res, some_failure);

        // Restore to success state.
        unsafe {
            console_run_script_locked(c"mock_success".as_ptr());
        }
    }

    /// Test and command callback in Rust
    #[test]
    fn command_and_test() {
        // If lastresult != zx_status!(Status::OK), it should return lastresult immediately.
        let some_failure = unsafe { console_run_script_locked(c"mock_failure".as_ptr()) };
        let res = unsafe { console_run_script_locked(c"and mock_success".as_ptr()) };
        expect_eq!(res, some_failure);

        // If lastresult == zx_status!(Status::OK), it should execute the command.
        unsafe {
            console_run_script_locked(c"mock_success".as_ptr());
        }
        let res = unsafe { console_run_script_locked(c"and mock_success".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));
    }

    /// Test repeat command callback in Rust
    #[test]
    fn command_repeat_test() {
        MOCK_CALL_COUNT.store(0, Ordering::Relaxed);

        // Repeat mock_success.
        let res = unsafe { console_run_script_locked(c"repeat 3 mock_success".as_ptr()) };
        expect_eq!(res, zx_status!(Status::OK));
        expect_eq!(MOCK_CALL_COUNT.load(Ordering::Relaxed), 3);

        MOCK_CALL_COUNT.store(0, Ordering::Relaxed);

        // Repeat with early failure.
        let res = unsafe { console_run_script_locked(c"repeat 3 mock_failure".as_ptr()) };
        expect_ne!(res, zx_status!(Status::OK));
        expect_eq!(MOCK_CALL_COUNT.load(Ordering::Relaxed), 1);
    }

    /// Test help command callback in Rust (normal)
    #[test]
    fn command_help_normal_test() {
        let cmd_ptr = unsafe { rust_console_match_command(c"help".as_ptr(), 0xff) };
        assert_nonnull!(cmd_ptr);
        let cb = unsafe { (*cmd_ptr).cmd_callback };

        // Statically allocate the capture buffer to avoid kernel stack overflows.
        static BUF: SyncUnsafeCell<[u8; 4096]> =
            SyncUnsafeCell(core::cell::UnsafeCell::new([0; 4096]));
        let buf_ptr = BUF.0.get() as *mut u8;

        // Reset buffer and start capturing stdout.
        unsafe {
            core::ptr::write_bytes(buf_ptr, 0, 4096);
            test_capture_stdout_start(buf_ptr, 4096);
        }
        // Execute command and stop capturing stdout.
        let res = unsafe { cb(1, core::ptr::null(), 0) };
        unsafe {
            test_capture_stdout_stop();
        }

        expect_eq!(res, zx_status!(Status::OK));

        // Slice the buffer up to the null terminator.
        let output = unsafe { core::slice::from_raw_parts(buf_ptr, 4096) };
        let len = output.iter().position(|&b| b == b'\0').unwrap_or(output.len());
        let output = &output[..len];

        expect_true!(contains_command_help(
            output,
            b"mock_avail_always",
            b"mock_avail_always help"
        ));
        expect_true!(contains_command_help(output, b"mock_success", b"mock_success help"));

        // Verify alphabetical sorting: mock_avail_always (a) comes before mock_success (s).
        let always_idx = find_command_index(output, b"mock_avail_always");
        let success_idx = find_command_index(output, b"mock_success");

        match (always_idx, success_idx) {
            (Some(a), Some(s)) => {
                expect_lt!(a, s);
            }
            _ => {
                record_failure!();
            }
        }
    }

    /// Test help command callback in Rust (panic)
    #[test]
    fn command_help_panic_test() {
        let cmd_ptr = unsafe { rust_console_match_command(c"help".as_ptr(), 0xff) };
        assert_nonnull!(cmd_ptr);
        let cb = unsafe { (*cmd_ptr).cmd_callback };

        // Statically allocate the capture buffer to avoid kernel stack overflows.
        static BUF: SyncUnsafeCell<[u8; 4096]> =
            SyncUnsafeCell(core::cell::UnsafeCell::new([0; 4096]));
        let buf_ptr = BUF.0.get() as *mut u8;

        // Reset buffer and start capturing stdout.
        unsafe {
            core::ptr::write_bytes(buf_ptr, 0, 4096);
            test_capture_stdout_start(buf_ptr, 4096);
        }
        // Execute command and stop capturing stdout.
        let res = unsafe { cb(1, core::ptr::null(), CMD_FLAG_PANIC) };
        unsafe {
            test_capture_stdout_stop();
        }

        expect_eq!(res, zx_status!(Status::OK));

        // Slice the buffer up to the null terminator.
        let output = unsafe { core::slice::from_raw_parts(buf_ptr, 4096) };
        let len = output.iter().position(|&b| b == b'\0').unwrap_or(output.len());
        let output = &output[..len];

        // mock_avail_always is still printed.
        expect_true!(contains_command_help(
            output,
            b"mock_avail_always",
            b"mock_avail_always help"
        ));

        // mock_success (CMD_AVAIL_NORMAL) should NOT be printed in panic mode.
        expect_false!(contains_command_help(output, b"mock_success", b"mock_success help"));
    }

    // Verifies that the test command is registered correctly from the Rust side.
    // We must expose this via FFI to ensure the C++ tests runner links this file.
    // The alternative would be to move these tests directly into
    // `zircon/kernel/lib/console/rust/src/lib.rs`, but keeping them in a separate
    // test crate avoids cluttering the main library with mocks and helpers.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn command_visibility_from_rust_test() -> bool {
        unsafe {
            let start = &__start_commands as *const Cmd;
            let stop = &__stop_commands as *const Cmd;
            let len = stop.offset_from(start) as usize;
            let commands = core::slice::from_raw_parts(start, len);

            for cmd in commands {
                if core::ffi::CStr::from_ptr(cmd.cmd_str) == c"mock_success" {
                    if core::ffi::CStr::from_ptr(cmd.help_str) != c"mock_success help" {
                        return false;
                    }
                    if cmd.availability_mask != CMD_AVAIL_NORMAL {
                        return false;
                    }
                    return true;
                }
            }
        }
        false
    }
}
