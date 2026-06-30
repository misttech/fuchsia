// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
// Allow missing safety doc comments for public unsafe extern FFI test helper functions.
#![allow(clippy::missing_safety_doc)]

#[cfg(not(console_enabled))]
use {console_rust as _, zx_status as _};

#[cfg(console_enabled)]
mod test {
    use console_rust::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use core::ffi::c_int;
    use zx_status::Status;

    macro_rules! zx_status {
        ($status:expr) => {
            $status.into_raw()
        };
    }

    // FFI imports.
    unsafe extern "C" {
        static __start_commands: console_rust::Cmd;
        static __stop_commands: console_rust::Cmd;
        fn strcmp(s1: *const core::ffi::c_char, s2: *const core::ffi::c_char) -> core::ffi::c_int;
    }

    // Statically registered commands.
    static_command!(
        TEST_CMD,
        c"mock_success".as_ptr(),
        c"mock_success help".as_ptr(),
        mock_success_callback,
        CMD_AVAIL_NORMAL
    );

    unsafe extern "C" fn mock_success_callback(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        zx_status!(Status::OK)
    }

    // FFI exports.

    // Verifies that the test command is registered correctly from the Rust side.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn command_visibility_from_rust_test() -> bool {
        unsafe {
            let start = &__start_commands as *const console_rust::Cmd;
            let stop = &__stop_commands as *const console_rust::Cmd;

            let mut current = start;
            let mut found = false;

            while current < stop {
                let cmd = &*current;
                if strcmp(cmd.cmd_str, c"mock_success".as_ptr()) == 0 {
                    found = true;
                    if strcmp(cmd.help_str, c"mock_success help".as_ptr()) != 0 {
                        found = false;
                    }
                    if cmd.availability_mask != CMD_AVAIL_NORMAL {
                        found = false;
                    }
                    break;
                }
                current = current.add(1);
            }

            found
        }
    }
}
