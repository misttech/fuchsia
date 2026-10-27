// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Memory console (`k`) commands (`mem`).

use core::ffi::c_int;
use kprint::kprintln;
use zx_status::Status;

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::thread::sleep_relative;
use crate::object::{NUM_LEVELS, PressureLevel, get_memory_watchdog};
use crate::platform_rs::timer::DurationMono;
use crate::vm::evictor::{EvictionLevel, Output, TriggerReason};
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::{pmm, scanner};

/// Allocate memory until the PMM returns `Status::NO_MEMORY`, then free it all.
///
/// The idea is to cause the system to briefly dip into an out of memory state.
fn cmd_oom_dip() -> c_int {
    pin_init::stack_pin_init!(let list = VmPageDoublyLinkedList::new());
    while pmm::alloc_pages(1, 0, list.as_mut()) != Err(Status::NO_MEMORY) {}
    // SAFETY: `list` contains valid allocated PMM pages.
    unsafe {
        pmm::free_list(list.as_mut());
    }
    kprintln!("allocated until ZX_ERR_NO_MEMORY (and then freed)");
    zx_status::sys::ZX_OK
}

fn cmd_oom(argc: c_int, argv: &[CmdArgs]) -> c_int {
    let mut rate: u64 = 0;
    let mut hard = false;
    if argc > 2 {
        if argv[2].as_str() == "dip" {
            return cmd_oom_dip();
        }

        if argv[2].as_str() == "hard" {
            hard = true;
        } else {
            rate = argv[2].arg_uint.saturating_mul(1024 * 1024) / (page::SIZE as u64);
        }
    }

    // When we reach the oom state the kernel may 'try harder' to reclaim memory and prevent us from
    // hitting OOM. To avoid this we disable the scanner (and evictor) to prevent additional memory
    // from becoming classified as evictable, and then evict anything that is already considered.
    kprintln!("Disabling VM scanner");
    scanner::push_disable_count();
    let pages_evicted = pmm::evictor()
        .evict_synchronous(
            /*min_mem_to_free=*/ u64::MAX,
            /*free_mem_target=*/ 0,
            EvictionLevel::IncludeNewest,
            Output::NoPrint,
            TriggerReason::Other,
        )
        .counts
        .non_loaned_total();
    if pages_evicted > 0 {
        kprintln!("Leaked {} pages from eviction", pages_evicted);
    }

    let mut pages_till_oom;
    // In case we are racing with someone freeing pages we will leak in a loop until we are sure
    // we have hit the oom state.
    while {
        pages_till_oom = get_memory_watchdog()
            .debug_num_bytes_till_pressure_level(PressureLevel::OutOfMemory)
            / (page::SIZE as u64);
        pages_till_oom > 0
    } {
        pin_init::stack_pin_init!(let list = VmPageDoublyLinkedList::new());
        if rate > 0 {
            let mut pages_leaked: u64 = 0;
            while pages_leaked < pages_till_oom {
                let alloc_pages = core::cmp::min(rate, pages_till_oom - pages_leaked);
                if pmm::alloc_pages(alloc_pages as usize, 0, list.as_mut()).is_ok() {
                    pages_leaked += alloc_pages;
                    kprintln!("Leaked {} pages", pages_leaked);
                }
                let _ = sleep_relative(DurationMono::from_seconds(1));
            }
        } else if pmm::alloc_pages(pages_till_oom as usize, 0, list.as_mut()).is_ok() {
            kprintln!("Leaked {} pages", pages_till_oom);
        }
        // We are deliberately leaking the allocated pages, so clear the list structure
        // without freeing the pages to avoid the empty assertion.
        list.as_mut().clear();
        // Ignore any errors under the assumption we had a racy allocation and try again next time
        // around the loop.
    }

    if hard {
        kprintln!("Continuing to leak pages forever");
        // Keep leaking as fast possible.
        loop {
            let _ = pmm::alloc_page(0);
        }
    }

    zx_status::sys::ZX_OK
}

fn cmd_usage(cmd_name: &str) -> c_int {
    kprintln!("usage:");
    kprintln!(
        "{:s} dump                                     : dump memory availability state info ",
        cmd_name
    );
    kprintln!(
        "{:s} oom [<rate>]                             : leak memory until OOM is triggered, \
         optionally specify the rate at which to leak (in MB per second). \
         This tries to reliably get the system into OOM state, so might disable some kinds of \
         kernel reclamation that prevent going into OOM. If you want to test system response \
         to memory pressure, unaltered from the default behavior, use avail_state instead.",
        cmd_name
    );
    kprintln!(
        "{:s} oom dip                                  : allocate until no mem, then free",
        cmd_name
    );
    kprintln!(
        "{:s} oom hard                                 : leak memory aggressively and keep on \
         leaking",
        cmd_name
    );
    kprintln!(
        "{:s} avail_state [step] <state> [<nsecs>] : allocate memory to go to memstate \
         <state>, hold the state for <nsecs> (10s by default). Only works if going to <state> \
         from current state requires allocating memory, can't free up pre-allocated memory. In \
         optional [step] mode, allocation pauses for 1 second at each intermediate memory \
         availability state until <state> is reached.",
        cmd_name
    );
    Status::INTERNAL.into_raw()
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where `argc >= 1`
/// and each element's `arg_str` is a valid null-terminated C string.
unsafe extern "C" fn cmd_mem(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console dispatcher guarantees `argc >= 1` and `argv` is non-null and points to
    // `argc` valid `CmdArgs` elements.
    let argv = unsafe { zr::slice_from_raw_parts(argv, argc as usize) };
    let name = argv[0].as_str();

    if argc < 2 {
        kprintln!("not enough arguments");
        return cmd_usage(name);
    }

    if argv[1].as_str() == "dump" {
        get_memory_watchdog().dump();
    } else if argv[1].as_str() == "oom" {
        if argc > 3 {
            return cmd_usage(name);
        }
        return cmd_oom(argc, argv);
    } else if argv[1].as_str() == "avail_state" {
        if argc < 3 {
            return cmd_usage(name);
        }
        let mut step = false;
        let mut index = 2usize;
        if argv[2].as_str() == "step" {
            step = true;
            index += 1;
        }

        let raw_state = argv[index].arg_uint;
        index += 1;
        let Some(state) =
            u8::try_from(raw_state).ok().and_then(|s| PressureLevel::try_from(s).ok())
        else {
            kprintln!(
                "Invalid memstate {}. Specify a value between 0 and {}.",
                raw_state,
                NUM_LEVELS - 1
            );
            return cmd_usage(name);
        };

        let mut pages_to_alloc;
        let mut pages_to_free: u64 = 0;
        pin_init::stack_pin_init!(let list = VmPageDoublyLinkedList::new());

        if step {
            let mut s = (NUM_LEVELS - 1) as u8;
            loop {
                // In case we are racing with someone freeing pages we will leak in a loop until we
                // are sure we have hit the required memory availability state.
                let mut pages_allocated: u64 = 0;
                let level = PressureLevel::try_from(s).unwrap();
                while {
                    pages_to_alloc = get_memory_watchdog()
                        .debug_num_bytes_till_pressure_level(level)
                        / (page::SIZE as u64);
                    pages_to_alloc > 0
                } {
                    if pmm::alloc_pages(pages_to_alloc as usize, 0, list.as_mut()).is_ok() {
                        kprintln!("Leaked {} pages", pages_to_alloc);
                        pages_allocated += pages_to_alloc;
                    }
                }
                pages_to_free += pages_allocated;
                if level == state {
                    break;
                }
                s -= 1;
                if pages_allocated != 0 {
                    kprintln!("Sleeping for 1 second...");
                    let _ = sleep_relative(DurationMono::from_seconds(1));
                }
            }
        } else {
            // In case we are racing with someone freeing pages we will leak in a loop until we are
            // sure we have hit the required memory availability state.
            while {
                pages_to_alloc = get_memory_watchdog().debug_num_bytes_till_pressure_level(state)
                    / (page::SIZE as u64);
                pages_to_alloc > 0
            } {
                if pmm::alloc_pages(pages_to_alloc as usize, 0, list.as_mut()).is_ok() {
                    kprintln!("Leaked {} pages", pages_to_alloc);
                    pages_to_free += pages_to_alloc;
                }
            }
        }

        if pages_to_free > 0 {
            let mut nsecs: u64 = 10;
            if (argc as usize) > index {
                nsecs = argv[index].arg_uint;
            }
            kprintln!("Sleeping for {} seconds...", nsecs);
            let _ = sleep_relative(DurationMono::from_seconds(nsecs as i64));
            // SAFETY: `list` contains valid allocated PMM pages.
            unsafe {
                pmm::free_list(list.as_mut());
            }
            kprintln!("Freed {} pages", pages_to_free);
        }
    } else {
        kprintln!("unknown command");

        return cmd_usage(name);
    }

    zx_status::sys::ZX_OK
}

static_command!(MEM_CMD, c"mem".as_ptr(), c"memory commands".as_ptr(), cmd_mem, CMD_AVAIL_NORMAL);
