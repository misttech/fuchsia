// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use console_env as _;

#[cfg(console_enabled)]
pub mod console {
    use console_env::BOOT_TEST_SUCCESS_STRING;
    use core::ffi::{c_char, c_int, c_void};
    use core::sync::atomic::{AtomicBool, Ordering};
    use zx_status::Status;

    // Converts a Status to its raw FFI representation for the C boundary.
    macro_rules! zx_status {
        ($status:expr) => {
            $status.into_raw()
        };
    }

    pub const CMD_AVAIL_NORMAL: u8 = 1 << 0;
    pub const CMD_AVAIL_PANIC: u8 = 1 << 1;
    pub const CMD_AVAIL_ALWAYS: u8 = CMD_AVAIL_NORMAL | CMD_AVAIL_PANIC;

    // Command is happening at crash time.
    pub const CMD_FLAG_PANIC: u32 = 1 << 0;

    pub static ECHO: AtomicBool = AtomicBool::new(true);
    pub static EXIT_CONSOLE: AtomicBool = AtomicBool::new(false);

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CmdArgs {
        pub arg_str: *const c_char,
        pub arg_uint: core::ffi::c_ulong,
        pub arg_ptr: *mut c_void,
        pub arg_int: core::ffi::c_long,
        pub arg_bool: bool,
    }

    pub type CmdCallback =
        unsafe extern "C" fn(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int;

    #[repr(C)]
    pub struct Cmd {
        pub cmd_str: *const c_char,
        pub help_str: *const c_char,
        pub cmd_callback: CmdCallback,
        pub availability_mask: u8,
    }

    // Safety: Cmd structures placed in the commands section are read-only
    // after boot and safe to share between threads.
    unsafe impl Sync for Cmd {}

    // Verify size and alignment compatibility with C++ structs.
    zr::static_assert!(core::mem::size_of::<CmdArgs>() == 40);
    zr::static_assert!(core::mem::align_of::<CmdArgs>() == 8);
    zr::static_assert!(core::mem::size_of::<Cmd>() == 32);
    zr::static_assert!(core::mem::align_of::<Cmd>() == 8);

    #[macro_export]
    macro_rules! commands_section {
        () => {
            ".data.rel.ro.commands"
        };
    }
    pub(crate) use commands_section;

    #[macro_export]
    macro_rules! static_command {
        ($var_name:ident, $cmd:expr, $help:expr, $func:expr, $mask:expr) => {
            #[used]
            #[unsafe(link_section = $crate::console_rust::console::commands_section!())]
            pub static $var_name: $crate::console_rust::console::Cmd =
                $crate::console_rust::console::Cmd {
                    cmd_str: $cmd,
                    help_str: $help,
                    cmd_callback: $func,
                    availability_mask: $mask,
                };
        };
    }
    pub(crate) use static_command;

    // FFI imports.
    unsafe extern "C" {
        // Used to print directly to the kernel console without stack-allocated formatting buffers.
        fn printf(format: *const c_char, ...) -> c_int;
        fn cpp_console_get_lastresult() -> c_int;
    }

    // Statically registered commands.
    static_command!(CMD_ECHO, c"echo".as_ptr(), core::ptr::null(), cmd_echo, CMD_AVAIL_ALWAYS);

    static_command!(
        CMD_EXIT,
        c"exit".as_ptr(),
        c"exit the command processor".as_ptr(),
        cmd_exit,
        CMD_AVAIL_NORMAL
    );

    static_command!(
        CMD_TEST,
        c"test".as_ptr(),
        c"test the command processor".as_ptr(),
        cmd_test,
        CMD_AVAIL_ALWAYS
    );

    static_command!(
        CMD_GRACEFUL_SHUTDOWN,
        c"graceful-shutdown".as_ptr(),
        c"shut the system down gracefully".as_ptr(),
        cmd_graceful_shutdown,
        CMD_AVAIL_ALWAYS
    );

    static_command!(
        CMD_BOOT_TEST_SUCCESS,
        c"boot-test-success".as_ptr(),
        c"report boot-test success".as_ptr(),
        cmd_boot_test_success,
        CMD_AVAIL_ALWAYS
    );

    static_command!(
        CMD_AND,
        c"and".as_ptr(),
        c"execute command if last command succeeded".as_ptr(),
        cmd_and,
        CMD_AVAIL_ALWAYS
    );

    static_command!(
        CMD_REPEAT,
        c"repeat".as_ptr(),
        c"execute command in a loop for N loops or until error".as_ptr(),
        cmd_repeat,
        CMD_AVAIL_ALWAYS
    );

    static_command!(CMD_HELP, c"help".as_ptr(), c"this list".as_ptr(), cmd_help, CMD_AVAIL_ALWAYS);

    // Callback implementations.
    unsafe extern "C" fn cmd_echo(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc > 1 {
            let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
            ECHO.store(args[1].arg_bool, Ordering::Relaxed);
        }
        zx_status!(Status::OK)
    }

    unsafe extern "C" fn cmd_exit(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
        rust_console_set_exit(true);
        zx_status!(Status::OK)
    }

    unsafe extern "C" fn cmd_test(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        unsafe { printf(c"argc %d, argv %p\n".as_ptr(), argc, argv) };
        for (i, arg) in args.iter().enumerate() {
            unsafe {
                printf(
                    c"\t%d: str '%s', int %ld, uint %#lx, ptr %p, bool %d\n".as_ptr(),
                    i as c_int,
                    arg.arg_str,
                    arg.arg_int,
                    arg.arg_uint,
                    arg.arg_ptr,
                    arg.arg_bool as c_int,
                )
            };
        }
        zx_status!(Status::OK)
    }

    unsafe extern "C" fn cmd_graceful_shutdown(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        // We use %c and format arguments here to prevent the compiler from optimizing the printf call
        // into puts or putchar, which are not defined in the kernel.
        unsafe {
            printf(
                c"%c** Performing graceful shutdown from kernel shell... ***\n".as_ptr(),
                b'*' as c_int,
            )
        };
        const ZX_SEC_10: i64 = 10 * 1_000_000_000;
        let dlog_deadline = crate::platform_rs::timer::current_mono_time() + ZX_SEC_10;
        if let Err(status) = crate::debuglog_rs::dlog_shutdown(dlog_deadline) {
            unsafe { printf(c"debuglog shutdown failed: %d\n".as_ptr(), status.into_raw()) };
            // Proceed to platform_halt() even if debuglog_rs::dlog_shutdown() fails.
        }
        // Does not return.
        crate::platform_rs::power::platform_halt(
            crate::platform_rs::power::PlatformHaltAction::Shutdown,
            crate::platform_rs::power::ZirconCrashReason::NoCrash,
        );
    }

    unsafe extern "C" fn cmd_boot_test_success(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        let last = unsafe { cpp_console_get_lastresult() };
        unsafe { printf(c"*** Last script command result: %d ***\n".as_ptr(), last) };
        if last == 0 {
            unsafe { printf(c"%s%c".as_ptr(), BOOT_TEST_SUCCESS_STRING.as_ptr(), b'\n' as c_int) };
        }
        last
    }

    unsafe fn get_commands() -> &'static [Cmd] {
        unsafe extern "C" {
            #[link_name = "__start_commands"]
            static __start_commands: Cmd;
            #[link_name = "__stop_commands"]
            static __stop_commands: Cmd;
        }
        let start = unsafe { &__start_commands as *const Cmd };
        let stop = unsafe { &__stop_commands as *const Cmd };
        let count = (stop as usize - start as usize) / core::mem::size_of::<Cmd>();
        unsafe { core::slice::from_raw_parts(start, count) }
    }

    unsafe fn match_command(name: *const c_char, availability_mask: u8) -> Option<&'static Cmd> {
        let commands = unsafe { get_commands() };
        let name_cstr = unsafe { core::ffi::CStr::from_ptr(name) };
        for cmd in commands {
            if (availability_mask & cmd.availability_mask) != 0 {
                let cmd_str_cstr = unsafe { core::ffi::CStr::from_ptr(cmd.cmd_str) };
                if cmd_str_cstr == name_cstr {
                    return Some(cmd);
                }
            }
        }
        None
    }

    unsafe extern "C" fn cmd_and(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc < 2 {
            unsafe { printf(c"Usage: and COMMAND...%c".as_ptr(), b'\n' as c_int) };
            return zx_status!(Status::INVALID_ARGS);
        }

        let last = unsafe { cpp_console_get_lastresult() };
        if last != 0 {
            return last;
        }

        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd = match unsafe { match_command(args[1].arg_str, CMD_AVAIL_NORMAL) } {
            Some(cmd) => cmd,
            None => {
                unsafe {
                    printf(c"command \"%s\" not found%c".as_ptr(), args[1].arg_str, b'\n' as c_int)
                };
                return zx_status!(Status::NOT_FOUND);
            }
        };

        unsafe { (cmd.cmd_callback)(argc - 1, argv.add(1), flags) }
    }

    unsafe extern "C" fn cmd_repeat(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        const MIN_ARGS: c_int = 3;
        if argc < MIN_ARGS {
            unsafe {
                printf(c"Usage: repeat <iterations | -1> COMMAND...%c".as_ptr(), b'\n' as c_int)
            };
            return zx_status!(Status::INVALID_ARGS);
        }

        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd = match unsafe { match_command(args[2].arg_str, CMD_AVAIL_NORMAL) } {
            Some(cmd) => cmd,
            None => {
                unsafe {
                    printf(c"command \"%s\" not found%c".as_ptr(), args[2].arg_str, b'\n' as c_int)
                };
                return zx_status!(Status::NOT_FOUND);
            }
        };

        // Negative arguments will cause it to effectively loop forever
        let iterations = if args[1].arg_int >= 0 { args[1].arg_uint as usize } else { usize::MAX };
        for i in 0..iterations {
            if iterations == usize::MAX {
                unsafe { printf(c"repeat (%zu): %s".as_ptr(), i + 1, args[2].arg_str) };
            } else {
                unsafe {
                    printf(c"repeat (%zu/%zu): %s".as_ptr(), i + 1, iterations, args[2].arg_str)
                };
            }
            for arg in MIN_ARGS..argc {
                unsafe { printf(c" %s".as_ptr(), args[arg as usize].arg_str) };
            }
            // We use %c and format arguments here to prevent the compiler from optimizing the printf
            // call into puts or putchar, which are not defined in the kernel.
            unsafe { printf(c"%c%s".as_ptr(), b'\n' as c_int, c"".as_ptr()) };

            let err = unsafe { (cmd.cmd_callback)(argc - 2, argv.add(2), flags) };
            if err != zx_status!(Status::OK) {
                unsafe {
                    printf(
                        c"stopping repeat due to nonzero status %d%c".as_ptr(),
                        err,
                        b'\n' as c_int,
                    )
                };
                return err;
            }
        }

        zx_status!(Status::OK)
    }

    unsafe extern "C" fn cmd_help(_argc: c_int, _argv: *const CmdArgs, flags: u32) -> c_int {
        let commands = unsafe { get_commands() };
        let count = commands.len();

        // Filter out commands based on if we're called at normal or panic time.
        let availability_mask =
            if (flags & CMD_FLAG_PANIC) != 0 { CMD_AVAIL_PANIC } else { CMD_AVAIL_NORMAL };

        unsafe {
            printf(c"command list:%c".as_ptr(), b'\n' as c_int);
        }

        // If we're not panicking (and are free to allocate memory), sort the
        // commands alphabetically before printing.
        if (flags & CMD_FLAG_PANIC) != 0 {
            for cmd in commands {
                if (availability_mask & cmd.availability_mask) != 0 && !cmd.help_str.is_null() {
                    unsafe {
                        printf(
                            c"\t%-16s: %s%c".as_ptr(),
                            cmd.cmd_str,
                            cmd.help_str,
                            b'\n' as c_int,
                        );
                    }
                }
            }
        } else {
            let mut ptrs_slice = match kalloc::Box::<[*const Cmd]>::try_new_zeroed_slice(count) {
                Ok(s) => s,
                Err(_) => return zx_status!(Status::NO_MEMORY),
            };
            for (i, cmd) in commands.iter().enumerate() {
                ptrs_slice[i] = cmd;
            }

            ptrs_slice.sort_unstable_by(|&a, &b| {
                let a_str = unsafe { core::ffi::CStr::from_ptr((*a).cmd_str) };
                let b_str = unsafe { core::ffi::CStr::from_ptr((*b).cmd_str) };
                a_str.cmp(b_str)
            });

            for &cmd_ptr in ptrs_slice.iter() {
                let cmd = unsafe { &*cmd_ptr };
                if (availability_mask & cmd.availability_mask) != 0 && !cmd.help_str.is_null() {
                    unsafe {
                        printf(
                            c"\t%-16s: %s%c".as_ptr(),
                            cmd.cmd_str,
                            cmd.help_str,
                            b'\n' as c_int,
                        );
                    }
                }
            }
        }

        zx_status!(Status::OK)
    }

    // FFI exports.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_console_get_echo() -> bool {
        ECHO.load(Ordering::Relaxed)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn rust_console_get_exit() -> bool {
        EXIT_CONSOLE.load(Ordering::Relaxed)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn rust_console_set_exit(val: bool) {
        EXIT_CONSOLE.store(val, Ordering::Relaxed);
    }

    /// # Safety
    ///
    /// `name` must be a valid pointer to a null-terminated C string.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_match_command(
        name: *const c_char,
        availability_mask: u8,
    ) -> *const Cmd {
        match unsafe { match_command(name, availability_mask) } {
            Some(cmd) => cmd,
            None => core::ptr::null(),
        }
    }
}
