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
    #[cfg(feature = "console_enable_history")]
    use core::sync::atomic::{AtomicPtr, AtomicUsize};
    use kprint::{kprint, kprintln};
    use zx_status::Status;

    pub const CMD_AVAIL_NORMAL: u8 = 1 << 0;
    pub const CMD_AVAIL_PANIC: u8 = 1 << 1;
    pub const CMD_AVAIL_ALWAYS: u8 = CMD_AVAIL_NORMAL | CMD_AVAIL_PANIC;

    // Command is happening at crash time.
    pub const CMD_FLAG_PANIC: u32 = 1 << 0;

    pub static ECHO: AtomicBool = AtomicBool::new(true);
    pub static EXIT_CONSOLE: AtomicBool = AtomicBool::new(false);

    #[cfg(feature = "console_enable_history")]
    const HISTORY_LEN: usize = 16;
    #[cfg(feature = "console_enable_history")]
    const LINE_LEN: usize = 128;

    #[cfg(feature = "console_enable_history")]
    static HISTORY_BUF: AtomicPtr<[u8; HISTORY_LEN * LINE_LEN]> =
        AtomicPtr::new(core::ptr::null_mut());
    #[cfg(feature = "console_enable_history")]
    static HISTORY_NEXT: AtomicUsize = AtomicUsize::new(0);

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CmdArgs {
        pub arg_str: *const c_char,
        pub arg_uint: core::ffi::c_ulong,
        pub arg_ptr: *mut c_void,
        pub arg_int: core::ffi::c_long,
        pub arg_bool: bool,
    }

    impl CmdArgs {
        pub fn as_str(&self) -> &str {
            if self.arg_str.is_null() {
                return "";
            }
            unsafe { core::ffi::CStr::from_ptr(self.arg_str) }.to_str().unwrap_or("")
        }
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

    impl Cmd {
        pub fn name(&self) -> &str {
            if self.cmd_str.is_null() {
                return "";
            }
            unsafe { core::ffi::CStr::from_ptr(self.cmd_str) }.to_str().unwrap_or("")
        }

        pub fn help(&self) -> &str {
            if self.help_str.is_null() {
                return "";
            }
            unsafe { core::ffi::CStr::from_ptr(self.help_str) }.to_str().unwrap_or("")
        }
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

    // Helpers.
    #[cfg(feature = "console_enable_history")]
    #[inline]
    fn ptrnext(ptr: usize) -> usize {
        (ptr + 1) % HISTORY_LEN
    }

    #[cfg(feature = "console_enable_history")]
    #[inline]
    fn ptrprev(ptr: usize) -> usize {
        (ptr + HISTORY_LEN - 1) % HISTORY_LEN
    }

    #[cfg(feature = "console_enable_history")]
    #[inline]
    fn try_get_history_buf_mut<'a>() -> Option<&'a mut [u8; HISTORY_LEN * LINE_LEN]> {
        let buf_ptr = HISTORY_BUF.load(Ordering::Relaxed);
        if buf_ptr.is_null() { None } else { Some(unsafe { &mut *buf_ptr }) }
    }

    #[cfg(feature = "console_enable_history")]
    #[inline]
    fn try_get_history_buf<'a>() -> Option<&'a [u8; HISTORY_LEN * LINE_LEN]> {
        let buf_ptr = HISTORY_BUF.load(Ordering::Relaxed);
        if buf_ptr.is_null() { None } else { Some(unsafe { &*buf_ptr }) }
    }

    /// Returns a slice of length `LINE_LEN` containing the history entry at index `line`.
    ///
    /// Note: Every `LINE_LEN` slot in `buf` is zero-initialized on allocation
    /// and explicitly null-terminated (`buf[start + LINE_LEN - 1] = 0`) whenever written
    /// by `rust_console_add_history`. Thus, the returned slice always contains a valid
    /// null-terminated C string.
    #[cfg(feature = "console_enable_history")]
    #[inline]
    fn history_line(buf: &[u8; HISTORY_LEN * LINE_LEN], line: usize) -> &[u8] {
        let start = line * LINE_LEN;
        &buf[start..start + LINE_LEN]
    }

    // FFI imports.
    unsafe extern "C" {
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

    #[cfg(feature = "console_enable_history")]
    static_command!(
        CMD_HISTORY,
        c"history".as_ptr(),
        c"command history".as_ptr(),
        cmd_history,
        CMD_AVAIL_ALWAYS
    );

    // Callback implementations.
    unsafe extern "C" fn cmd_echo(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc > 1 {
            let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
            ECHO.store(args[1].arg_bool, Ordering::Relaxed);
        }
        zx_status::sys::ZX_OK
    }

    unsafe extern "C" fn cmd_exit(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
        rust_console_set_exit(true);
        zx_status::sys::ZX_OK
    }

    unsafe extern "C" fn cmd_test(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        kprintln!("argc {}, argv {:p}", argc, argv);
        for (i, arg) in args.iter().enumerate() {
            kprintln!(
                "\t{}: str '{:cs}', int {}, uint {:#x}, ptr {:p}, bool {}",
                i,
                arg.arg_str,
                arg.arg_int,
                arg.arg_uint,
                arg.arg_ptr,
                arg.arg_bool,
            );
        }
        zx_status::sys::ZX_OK
    }

    unsafe extern "C" fn cmd_graceful_shutdown(
        _argc: c_int,
        _argv: *const CmdArgs,
        _flags: u32,
    ) -> c_int {
        kprintln!("*** Performing graceful shutdown from kernel shell... ***");
        const ZX_SEC_10: i64 = 10 * 1_000_000_000;
        let dlog_deadline = crate::platform_rs::timer::current_mono_time() + ZX_SEC_10;
        if let Err(status) = crate::debuglog_rs::dlog_shutdown(dlog_deadline) {
            kprintln!("debuglog shutdown failed: {}", status.into_raw());
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
        kprintln!("*** Last script command result: {} ***", last);
        if last == 0 {
            kprintln!("{:s}", BOOT_TEST_SUCCESS_STRING);
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

    /// # Safety
    ///
    /// `name` must be a valid pointer to a null-terminated C string.
    pub unsafe fn match_command(
        name: *const c_char,
        availability_mask: u8,
    ) -> Option<&'static Cmd> {
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
            kprintln!("Usage: and COMMAND...");
            return Status::INVALID_ARGS.into_raw();
        }

        let last = unsafe { cpp_console_get_lastresult() };
        if last != 0 {
            return last;
        }

        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd = match unsafe { match_command(args[1].arg_str, CMD_AVAIL_NORMAL) } {
            Some(cmd) => cmd,
            None => {
                kprintln!("command \"{:cs}\" not found", args[1].arg_str);
                return Status::NOT_FOUND.into_raw();
            }
        };

        unsafe { (cmd.cmd_callback)(argc - 1, argv.add(1), flags) }
    }

    unsafe extern "C" fn cmd_repeat(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        const MIN_ARGS: c_int = 3;
        if argc < MIN_ARGS {
            kprintln!("Usage: repeat <iterations | -1> COMMAND...");
            return Status::INVALID_ARGS.into_raw();
        }

        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd = match unsafe { match_command(args[2].arg_str, CMD_AVAIL_NORMAL) } {
            Some(cmd) => cmd,
            None => {
                kprintln!("command \"{:cs}\" not found", args[2].arg_str);
                return Status::NOT_FOUND.into_raw();
            }
        };

        // Negative arguments will cause it to effectively loop forever
        let iterations = if args[1].arg_int >= 0 { args[1].arg_uint as usize } else { usize::MAX };
        for i in 0..iterations {
            if iterations == usize::MAX {
                kprint!("repeat ({}): {:cs}", i + 1, args[2].arg_str);
            } else {
                kprint!("repeat ({}/{}): {:cs}", i + 1, iterations, args[2].arg_str);
            }
            for arg in MIN_ARGS..argc {
                kprint!(" {:cs}", args[arg as usize].arg_str);
            }
            kprintln!("");

            let err = unsafe { (cmd.cmd_callback)(argc - 2, argv.add(2), flags) };
            if err != zx_status::sys::ZX_OK {
                kprintln!("stopping repeat due to nonzero status {err}");
                return err;
            }
        }

        zx_status::sys::ZX_OK
    }

    unsafe extern "C" fn cmd_help(_argc: c_int, _argv: *const CmdArgs, flags: u32) -> c_int {
        let commands = unsafe { get_commands() };
        let count = commands.len();

        // Filter out commands based on if we're called at normal or panic time.
        let availability_mask =
            if (flags & CMD_FLAG_PANIC) != 0 { CMD_AVAIL_PANIC } else { CMD_AVAIL_NORMAL };

        kprintln!("command list:");

        // If we're not panicking (and are free to allocate memory), sort the
        // commands alphabetically before printing.
        if (flags & CMD_FLAG_PANIC) != 0 {
            for cmd in commands {
                if (availability_mask & cmd.availability_mask) != 0 && !cmd.help_str.is_null() {
                    kprintln!("\t{:<16cs}: {:cs}", cmd.cmd_str, cmd.help_str);
                }
            }
        } else {
            let mut ptrs_slice = match kalloc::Box::<[*const Cmd]>::try_new_zeroed_slice(count) {
                Ok(s) => s,
                Err(_) => return Status::NO_MEMORY.into_raw(),
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
                    kprintln!("\t{:<16cs}: {:cs}", cmd.cmd_str, cmd.help_str);
                }
            }
        }

        zx_status::sys::ZX_OK
    }

    #[cfg(feature = "console_enable_history")]
    unsafe extern "C" fn cmd_history(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
        let Some(buf) = try_get_history_buf() else {
            return zx_status::sys::ZX_OK;
        };

        kprintln!("command history:");

        // In C++, the history buffer was accessed using raw pointer arithmetic. In Rust, we opt to
        // index directly into the flat 1D array slice reference. Checking `buf[ptr * LINE_LEN] != 0`
        // identifies active entries, after which `history_line` safely slices each entry as a C-style
        // string pointer.
        let history_next = HISTORY_NEXT.load(Ordering::Relaxed);
        let mut ptr = ptrprev(history_next);
        for _ in 0..HISTORY_LEN {
            if buf[ptr * LINE_LEN] != 0 {
                let str_ptr = history_line(buf, ptr).as_ptr() as *const c_char;
                kprintln!("\t{:cs}", str_ptr);
            }
            ptr = ptrprev(ptr);
        }

        zx_status::sys::ZX_OK
    }

    /// # Safety
    ///
    /// `line` must point to a valid null-terminated C string.
    ///
    /// # Concurrency Notes
    ///
    /// While the atomic `HISTORY_BUF` and `HISTORY_NEXT` ensure safe visibility of the ring buffer
    /// pointer and index across threads, the actual `copy_from_slice` writes into `HISTORY_BUF` are
    /// non-atomic and performed without external locking. This preserves the 1:1 concurrency model of
    /// the C++ implementation, which assumes a single active serial input stream or interactive console
    /// loop. If concurrent multi-writer console lines are introduced in the future, writing to and
    /// traversing history would require external synchronization.
    #[cfg(feature = "console_enable_history")]
    pub unsafe fn add_history(line: *const c_char) {
        let Some(buf) = try_get_history_buf_mut() else {
            return;
        };

        if line.is_null() {
            return;
        }

        // Reject empty lines.
        let line_cstr = unsafe { core::ffi::CStr::from_ptr(line) };
        let line_bytes = line_cstr.to_bytes_with_nul();
        if line_bytes.len() <= 1 {
            // Just the null terminator.
            return;
        }

        let history_next = HISTORY_NEXT.load(Ordering::Relaxed);
        let last = ptrprev(history_next);
        let prev_slice = history_line(buf, last);
        if let Ok(prev_cstr) = core::ffi::CStr::from_bytes_until_nul(prev_slice)
            && line_cstr == prev_cstr
        {
            // Don't store duplicate commands in history.
            return;
        }

        // Compute next line start index and store.
        let next_start = history_next * LINE_LEN;
        let copy_len = core::cmp::min(line_bytes.len(), LINE_LEN);
        buf[next_start..next_start + copy_len].copy_from_slice(&line_bytes[..copy_len]);
        buf[next_start + LINE_LEN - 1] = 0;

        HISTORY_NEXT.store(ptrnext(history_next), Ordering::Relaxed);
    }

    #[cfg(feature = "console_enable_history")]
    pub fn start_history_cursor() -> usize {
        if try_get_history_buf().is_none() {
            return 0;
        }

        let history_next = HISTORY_NEXT.load(Ordering::Relaxed);
        ptrprev(history_next)
    }

    #[cfg(feature = "console_enable_history")]
    pub fn next_history(cursor: &mut usize) -> *const c_char {
        let Some(buf) = try_get_history_buf() else {
            return c"".as_ptr();
        };

        *cursor %= HISTORY_LEN;
        let history_next = HISTORY_NEXT.load(Ordering::Relaxed);
        let i = ptrnext(*cursor);

        if i == history_next {
            // Don't let the cursor hit the head.
            return c"".as_ptr();
        }

        *cursor = i;
        history_line(buf, i).as_ptr() as *const c_char
    }

    /// # Traversal Behavior
    ///
    /// Preserves 1:1 C++ behavior: Returns the history command at `*cursor` and steps the cursor
    /// backward to the previous item. Because `start_history_cursor()` initializes `*cursor` at
    /// the newest entry, the first Up-Arrow keypress returns the newest command without skipping it.
    #[cfg(feature = "console_enable_history")]
    pub fn prev_history(cursor: &mut usize) -> *const c_char {
        let Some(buf) = try_get_history_buf() else {
            return c"".as_ptr();
        };

        *cursor %= HISTORY_LEN;
        let history_next = HISTORY_NEXT.load(Ordering::Relaxed);
        let str_ptr = history_line(buf, *cursor).as_ptr() as *const c_char;

        // If we are already at head, stop here.
        if *cursor == history_next {
            return str_ptr;
        }

        // Back up one.
        let i = ptrprev(*cursor);

        // If the next one is null, stop here.
        if buf[i * LINE_LEN] == 0 {
            return str_ptr;
        }

        *cursor = i;
        str_ptr
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

    /// # Safety
    ///
    /// Safe to call at any time from any thread or C FFI caller.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_console_is_history_enabled() -> bool {
        cfg!(feature = "console_enable_history")
    }

    /// # Safety
    ///
    /// This function should only be called once during initialization.
    #[cfg(feature = "console_enable_history")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_init_history() {
        // This buffer should NOT be initialized more than once in production code, but run this check
        // to avoid leaking memory.
        if let Some(buf) = try_get_history_buf_mut() {
            // If buffer already exists, reuse existing buffer allocation and reset entries. This use
            // case should only appear in tests.
            buf.fill(0);
            HISTORY_NEXT.store(0, Ordering::Relaxed);
            return;
        }

        // Allocate and set up the history buffer.
        if let Ok(buf) = kalloc::Box::<[u8; HISTORY_LEN * LINE_LEN]>::try_new_zeroed() {
            let ptr = kalloc::Box::into_raw(unsafe { buf.assume_init() });
            HISTORY_BUF.store(ptr, Ordering::Relaxed);
            HISTORY_NEXT.store(0, Ordering::Relaxed);
        }
    }

    /// # Safety
    ///
    /// This function should only be called once during initialization.
    #[cfg(not(feature = "console_enable_history"))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_init_history() {}

    /// # Safety
    ///
    /// `line` must be a valid pointer to a null-terminated C string.
    #[cfg(feature = "console_enable_history")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_add_history(line: *const c_char) {
        unsafe { add_history(line) }
    }

    /// # Safety
    ///
    /// No special safety requirements, `_line` is unused.
    #[cfg(not(feature = "console_enable_history"))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_add_history(_line: *const c_char) {}

    /// # Safety
    ///
    /// No special safety requirements, but marked unsafe to match C ABI export.
    #[cfg(feature = "console_enable_history")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_start_history_cursor() -> u32 {
        start_history_cursor() as u32
    }

    /// # Safety
    ///
    /// No special safety requirements, but marked unsafe to match C ABI export.
    #[cfg(not(feature = "console_enable_history"))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_start_history_cursor() -> u32 {
        0
    }

    /// # Safety
    ///
    /// `cursor` must point to a valid `u32` initialized by `rust_console_start_history_cursor`.
    #[cfg(feature = "console_enable_history")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_next_history(cursor: *mut u32) -> *const c_char {
        if cursor.is_null() {
            return c"".as_ptr();
        }
        let mut c = unsafe { *cursor as usize };
        let res = next_history(&mut c);
        unsafe { *cursor = c as u32 };
        res
    }

    /// # Safety
    ///
    /// No special safety requirements, `_cursor` is unused.
    #[cfg(not(feature = "console_enable_history"))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_next_history(_cursor: *mut u32) -> *const c_char {
        c"".as_ptr()
    }

    /// # Safety
    ///
    /// `cursor` must point to a valid `u32` initialized by `rust_console_start_history_cursor`.
    #[cfg(feature = "console_enable_history")]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_prev_history(cursor: *mut u32) -> *const c_char {
        if cursor.is_null() {
            return c"".as_ptr();
        }
        let mut c = unsafe { *cursor as usize };
        let res = prev_history(&mut c);
        unsafe { *cursor = c as u32 };
        res
    }

    /// # Safety
    ///
    /// No special safety requirements, `_cursor` is unused.
    #[cfg(not(feature = "console_enable_history"))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_console_prev_history(_cursor: *mut u32) -> *const c_char {
        c"".as_ptr()
    }
}
