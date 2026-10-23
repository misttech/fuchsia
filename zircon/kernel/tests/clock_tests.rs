// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Monotonic clock and cycle counter diagnostic console command.

use core::ffi::c_int;
use kprint::kprintln;

use crate::console::CmdArgs;
use crate::kernel::mp::{SMP_MAX_CPUS, cpu_num_to_mask, is_cpu_online};
use crate::kernel::thread::{ThreadPtr, sleep_relative};
use crate::kernel::types::cpu_num_t;
use crate::platform_rs::timer::{DurationMono, current_mono_time};

/// # Safety
///
/// `_argv` is not dereferenced; `unsafe` only to match the console command callback signature.
pub(super) unsafe extern "C" fn clock_tests(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    let _ = sleep_relative(DurationMono::from_millis(100));
    let mut c = libarch::cycles();
    let _ = current_mono_time();
    c = libarch::cycles().wrapping_sub(c);
    kprintln!("{} cycles per current_mono_time()", c);

    kprintln!("making sure time never goes backwards");
    {
        kprintln!("testing current_mono_time()");
        let start = current_mono_time();
        let mut last = start;
        loop {
            let t2 = current_mono_time();
            // printf("%llu %llu\n", last, t2);
            if t2 < last {
                kprintln!(
                    "WARNING: time ran backwards: {} < {}",
                    t2.into_nanos(),
                    last.into_nanos()
                );
                last = t2;
                continue;
            }
            last = t2;
            if last - start > DurationMono::from_seconds(5) {
                break;
            }
        }
    }

    kprintln!("counting to 5, in one second intervals");
    for i in 0..5 {
        let _ = sleep_relative(DurationMono::from_seconds(1));
        kprintln!("{}", i + 1);
    }

    // SAFETY: Called from normal thread context after threading initialization.
    let old_affinity = unsafe { ThreadPtr::current().get_cpu_affinity() };

    for cpu in 0..(SMP_MAX_CPUS as cpu_num_t) {
        if !is_cpu_online(cpu) {
            continue;
        }

        kprintln!("measuring cpu clock against current_mono_time() on cpu {}", cpu);

        // SAFETY: Called from normal thread context after threading initialization.
        unsafe {
            ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(cpu));
        }

        for _ in 0..3 {
            let mut cycles = libarch::cycles();
            let start = current_mono_time();
            while (current_mono_time() - start) < DurationMono::from_seconds(1) {}
            cycles = libarch::cycles().wrapping_sub(cycles);
            kprintln!("cpu {}: {} cycles per second", cpu, cycles);
        }
    }

    // SAFETY: Called from normal thread context after threading initialization.
    unsafe {
        ThreadPtr::current().set_cpu_affinity(old_affinity);
    }

    0
}
