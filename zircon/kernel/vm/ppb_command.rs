// Copyright 2021 The Fuchsia Authors
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::event::Event;
use crate::kernel::thread::{self, LOW_PRIORITY, ThreadPtr};
use crate::platform_rs::timer::{DurationMono, InstantMono};
use crate::vm::physical_page_borrowing_config::PhysicalPageBorrowingConfig;
use crate::vm::pmm;
use core::ffi::{c_int, c_void};
use core::pin::Pin;
use core::ptr;
use kprint::kprintln;
use ksync::{KCell, declare_singleton_mutex, lock};
use lazy_init::LazyInit;
use zx_status::Status;

declare_singleton_mutex!(PpbStatsLock);
static PPB_STATS_THREAD: KCell<Option<ThreadPtr>, PpbStatsLock> = KCell::new(None);
/// Initialized in place by `ppb_stats_init_func`, since an `Event` must be pinned.
static PPB_STATS_THREAD_STOP_EVENT: LazyInit<Event> = LazyInit::uninit();

fn ppb_stats_init_func(_level: init::LkInitLevel) {
    // SAFETY: Single-threaded initialization during early boot, before any console command can
    // access the event.
    unsafe {
        let _ = Pin::static_ref(&PPB_STATS_THREAD_STOP_EVENT).init_pin(Event::init_unsignaled());
    }
}

init::lk_init_hook!(ppb_stats_init, ppb_stats_init_func, init::LK_INIT_LEVEL_EARLIEST);

fn cmd_ppb_borrowing_on() {
    PhysicalPageBorrowingConfig::get().set_borrowing_on_mru_enabled(true);
    kprintln!("borrowing enabled");
}

fn cmd_ppb_borrowing_off() {
    PhysicalPageBorrowingConfig::get().set_borrowing_on_mru_enabled(false);
    kprintln!("borrowing disabled");
}

fn cmd_ppb_loaning_on() {
    PhysicalPageBorrowingConfig::get().set_loaning_enabled(true);
    kprintln!("loaning enabled");
}

fn cmd_ppb_loaning_off() {
    PhysicalPageBorrowingConfig::get().set_loaning_enabled(false);
    kprintln!("loaning disabled");
}

fn cmd_ppb_stats() {
    pmm::print_physical_page_borrowing_stats();
}

extern "C" fn ppb_stats_thread(_arg: *mut c_void) -> i32 {
    loop {
        cmd_ppb_stats();
        let status = PPB_STATS_THREAD_STOP_EVENT
            .wait(&Deadline::after_mono(DurationMono::from_seconds(1), TimerSlack::none()));
        if status.is_ok() {
            return 0;
        }
        debug_assert!(status == Err(Status::TIMED_OUT));
    }
}

fn cmd_ppb_stats_on() {
    let local_ppb_stats_thread;
    {
        // scope guard
        lock!(let mut guard = PpbStatsLock::lock());
        let token = guard.token_mut();
        // SAFETY: `ppb_stats_thread` is a valid thread entry point that ignores its argument.
        let stats_thread = unsafe {
            thread::create_with_priority(
                c"ppb-stats-thread".as_ptr(),
                ppb_stats_thread,
                ptr::null_mut(),
                LOW_PRIORITY,
            )
        };
        assert!(stats_thread.is_ok());
        let stats_thread = stats_thread.unwrap();
        // SAFETY: `PpbStatsLock` is a singleton mutex and `token` proves it is held.
        unsafe {
            *PPB_STATS_THREAD.get_mut(token) = Some(stats_thread);
        }
        local_ppb_stats_thread = stats_thread;
    } // ~guard
    // SAFETY: The thread was just created above. Console commands are serialized by the console
    // lock, so `cmd_ppb_stats_off` cannot have joined it yet.
    unsafe {
        local_ppb_stats_thread.resume();
    }
}

fn cmd_ppb_stats_off() {
    let local_ppb_stats_thread;
    {
        // scope guard
        lock!(let mut guard = PpbStatsLock::lock());
        let token = guard.token_mut();
        // SAFETY: `PpbStatsLock` is a singleton mutex and `token` proves it is held.
        local_ppb_stats_thread = unsafe { PPB_STATS_THREAD.get_mut(token) }.take();
    } // ~guard
    PPB_STATS_THREAD_STOP_EVENT.signal();
    let local_ppb_stats_thread = local_ppb_stats_thread.expect("ppb stats thread is not running");
    // SAFETY: The thread was taken out of `PPB_STATS_THREAD` under `PpbStatsLock`, so no other
    // caller can join it.
    let retcode = unsafe { local_ppb_stats_thread.join(InstantMono::INFINITE) };
    debug_assert!(retcode == Ok(0));
    let _ = PPB_STATS_THREAD_STOP_EVENT.unsignal();
}

type CmdFunc = fn();
struct Cmd {
    name: &'static str,
    func: CmdFunc,
}

static COMMANDS: [Cmd; 7] = [
    Cmd { name: "borrowing_on", func: cmd_ppb_borrowing_on },
    Cmd { name: "borrowing_off", func: cmd_ppb_borrowing_off },
    Cmd { name: "loaning_on", func: cmd_ppb_loaning_on },
    Cmd { name: "loaning_off", func: cmd_ppb_loaning_off },
    Cmd { name: "stats", func: cmd_ppb_stats },
    Cmd { name: "stats_on", func: cmd_ppb_stats_on },
    Cmd { name: "stats_off", func: cmd_ppb_stats_off },
];

/// ```text
/// k ppb borrowing_on
///   * this is the default on boot
///   * enables page borrowing for new allocations
///   * see also k ppb borrowing_off
/// k ppb borrowing_off
///   * disables page borrowing for new allocations
///   * see also k ppb borrowing_on
/// k ppb loaning_on
///   * enables loaning when a contiguous VMO pages are decommitted
/// k ppb loaning_off
///   * disables loaning when a contiguous VMO pages are decommitted
/// k ppb stats
///   * output ppb-related stats (once)
/// k ppb stats_on
///   * repeatedly output ppb-relevant stats (fairly frequently, for observing usage scenarios)
/// k ppb stats_off
///   * stop repeatedly outputting ppb-relevant stats
/// ```
///
/// # Safety
///
/// `argv` must be non-null and point to `argc` valid `CmdArgs` structures.
unsafe extern "C" fn cmd_ppb(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc != 2 {
        kprintln!("2 arguments expected");
        kprintln!("usage:");
        kprintln!("ppb <cmd>");
        kprintln!("command list:");
        for cmd in &COMMANDS {
            kprintln!("{:s}", cmd.name);
        }
        return -1;
    }

    // SAFETY: The caller guarantees `argv` is non-null and points to `argc` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    for cmd in &COMMANDS {
        if argv[1].as_str() == cmd.name {
            (cmd.func)();
            return 0;
        }
    }
    kprintln!("sub-command not found - available sub-commands:");
    for cmd in &COMMANDS {
        kprintln!("{:s}", cmd.name);
    }
    -1
}

static_command!(
    CMD_PPB,
    c"ppb".as_ptr(),
    c"control contiguous physical page borrowing".as_ptr(),
    cmd_ppb,
    CMD_AVAIL_NORMAL
);
