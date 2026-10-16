// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Undefined Behavior commands.
//! These commands can be used to test the undefined behavior sanitizer.
//! A kernel compiled with the `kubsan` variant should be able to detect
//! each of them.

#![cfg(ubsan)]

use boot_options::{BootOptions, CheckFailAction};
use core::ffi::c_int;
use kprint::kprintln;
use zx_status::Status;

use super::debugcommands_ffi::{
    cpp_ub_array_oob, cpp_ub_invalid_builtin_clz, cpp_ub_invalid_builtin_ctz,
    cpp_ub_misaligned_ptr, cpp_ub_nonnull_return, cpp_ub_nullability_return, cpp_ub_overflow_ptr,
    cpp_ub_overflow_signed_int_add, cpp_ub_overflow_signed_int_shift, cpp_ub_unaligned_assumption,
    cpp_ub_undefined_bool, cpp_ub_undefined_enum, cpp_ub_unreachable,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};

struct UndefinedBehaviorCommand {
    name: &'static str,
    func: Option<unsafe extern "C" fn()>,
    description: &'static str,
    cannot_continue: bool,
}

const UB_COMMANDS: [UndefinedBehaviorCommand; 14] = [
    UndefinedBehaviorCommand {
        name: "all",
        func: None,
        description: "run each subcommand in turn (requires kernel.ubsan.panic=false)",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "array_oob",
        func: Some(cpp_ub_array_oob),
        description: "array out of bounds access",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "invalid_builtin_clz",
        func: Some(cpp_ub_invalid_builtin_clz),
        description: "call __builtin_clz with 0",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "invalid_builtin_ctz",
        func: Some(cpp_ub_invalid_builtin_ctz),
        description: "call __builtin_ctz with 0",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "misaligned_ptr",
        func: Some(cpp_ub_misaligned_ptr),
        description: "use a misaligned pointer",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "nonnull_return",
        func: Some(cpp_ub_nonnull_return),
        description: "return nullptr from returns_nonnull function",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "nullability_return",
        func: Some(cpp_ub_nullability_return),
        description: "return nullptr from _Nonnull function",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "overflow_ptr",
        func: Some(cpp_ub_overflow_ptr),
        description: "pointer arithmetic that overflows",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "overflow_signed_int_add",
        func: Some(cpp_ub_overflow_signed_int_add),
        description: "signed integer addition that overflows",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "overflow_signed_int_shift",
        func: Some(cpp_ub_overflow_signed_int_shift),
        description: "signed integer shift that overflows",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "unaligned_assumption",
        func: Some(cpp_ub_unaligned_assumption),
        description: "make a wrong alignment assumption",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "undefined_enum",
        func: Some(cpp_ub_undefined_enum),
        description: "use an undefined value in a enum",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "undefined_bool",
        func: Some(cpp_ub_undefined_bool),
        description: "use a bool that is not true nor false",
        cannot_continue: false,
    },
    UndefinedBehaviorCommand {
        name: "unreachable",
        func: Some(cpp_ub_unreachable),
        description: "execute unreachable code.",
        cannot_continue: true,
    },
];

const MAX_COMMAND_NAME_SIZE: usize = {
    let mut size = 0;
    let mut i = 0;
    while i < UB_COMMANDS.len() {
        let len = UB_COMMANDS[i].name.len();
        if len > size {
            size = len;
        }
        i += 1;
    }
    size
};
zr::static_assert!(MAX_COMMAND_NAME_SIZE == 25);

fn cmd_usage(cmd_name: &str) -> c_int {
    kprintln!("usage:");
    for ub_cmd in &UB_COMMANDS {
        kprintln!("{:s} {:<25s} : {:s}", cmd_name, ub_cmd.name, ub_cmd.description);
    }
    Status::INTERNAL.into_raw()
}

/// # Safety
///
/// `argv` must point to `argc` valid `CmdArgs` elements.
unsafe extern "C" fn cmd_ub(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let name = args[0].as_str();
    if argc != 2 {
        kprintln!("Exactly one argument required.");
        return cmd_usage(name);
    }

    let subcommand = args[1].as_str();
    for ub_cmd in &UB_COMMANDS {
        if ub_cmd.name == subcommand {
            if let Some(func) = ub_cmd.func {
                // SAFETY: Intentionally triggers the requested C++ UBSan test helper.
                unsafe { func() };
            } else {
                for next_ub_cmd in &UB_COMMANDS[1..] {
                    if BootOptions::get().ubsan_action == CheckFailAction::Oops
                        && next_ub_cmd.cannot_continue
                    {
                        kprintln!(
                            "*** Skipping `ub {:s}`, which cannot avoid panic ***",
                            next_ub_cmd.name
                        );
                    } else if let Some(func) = next_ub_cmd.func {
                        kprintln!("*** ub {:s}", next_ub_cmd.name);
                        // SAFETY: Intentionally triggers the C++ UBSan test helper.
                        unsafe { func() };
                    }
                }
            }
            return 0;
        }
    }

    cmd_usage(name)
}

static_command!(
    UB_CMD,
    c"ub".as_ptr(),
    c"trigger undefined behavior".as_ptr(),
    cmd_ub,
    CMD_AVAIL_NORMAL
);
