// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub mod cache;
mod crashlog;
mod feature;
mod fpu;
mod mp;
mod periphmap;
mod registers;
pub mod restricted;
pub mod smccc;
mod spinlock;
#[cfg(console_enabled)]
mod sysreg;
mod thread;
mod uarch;
mod user_copy;
pub mod vm;

use crate::kernel::types::cpu_num_t;
use arch_arm64_aspace_bindings as aspace_bindings;

unsafe extern "C" {
    fn cpp_arm64_cpu_num_to_mpidr(cpu_num: cpu_num_t) -> u64;
}

/// The maximum cache line seen on any known ARM hardware.
pub const MAX_CACHE_LINE: usize = 64;
#[repr(align(64))]
pub struct CpuAlignMarker;

/// Translates a CPU number back to the MPIDR of that CPU.
///
/// Mirrors `arch_cpu_num_to_mpidr()` from
/// `zircon/kernel/arch/arm64/include/arch/arm64/mp.h`.
#[inline]
pub fn cpu_num_to_mpidr(cpu_num: cpu_num_t) -> u64 {
    // SAFETY: `cpp_arm64_cpu_num_to_mpidr` is a read-only FFI call into the arm64 percpu
    // topology table with no side effects. Out of range CPU numbers are handled by the C++
    // implementation.
    unsafe { cpp_arm64_cpu_num_to_mpidr(cpu_num) }
}

pub use libarch::arm64::{device_memory_barrier, isb, thread_memory_barrier, wfi};
pub use periphmap::periph_paddr_to_vaddr;
pub use user_copy::{
    arch_copy_from_user, arch_copy_from_user_capture_faults, arch_copy_to_user,
    arch_copy_to_user_capture_faults,
};

// [arm/v8]: C5.2.19 CPSR / D13.2.112 SPSR_EL1
pub const ARM_NCZV_FLAGS: u32 = 0xf000_0000;
// [arm/v8]: FEAT_BTI (Branch Target Identification) branch type flags.
pub const ARM64_BTYPE_FLAGS: u32 = 0x0000_0c00;
// [arm/v8]: C5.2.19 CPSR M[4] / nRW bit indicating AArch32 execution state.
pub const ARM32_BIT_MODE: u32 = 1 << 4; // 0x10
// [arm/v8]: C5.2.19 CPSR T bit indicating AArch32 Thumb execution state.
pub const ARM32_BIT_THUMB_MODE: u32 = 1 << 5; // 0x20
// [arm/v8]: C5.2.19 CPSR GE[19:16] and Q[27] bits.
pub const ARM32_GE_Q_BITS: u32 = 0x080f_0000;
// [arm/v8]: C5.2.19 CPSR IT[15:10, 26:25] If-Then execution state bits.
pub const ARM32_IT_BITS: u32 = 0x0600_fc00;

pub const ARM64_USER_RESTRICTED_VISIBLE_FLAGS: u32 = ARM_NCZV_FLAGS | ARM64_BTYPE_FLAGS; // 0xf000_0c00
pub const ARM32_USER_RESTRICTED_VISIBLE_FLAGS: u32 =
    ARM_NCZV_FLAGS | ARM32_BIT_MODE | ARM32_BIT_THUMB_MODE | ARM32_GE_Q_BITS | ARM32_IT_BITS; // 0xfe0f_fc30
pub const ARM32_BIT_MODE_REGISTER_COUNT: usize = 15;

#[inline]
fn arm64_feature_test(feature: u32) -> bool {
    // SAFETY: Only the boot CPU's `arm64_feature_init` writes the mask, before any other CPU
    // starts, so no write races with this read.
    unsafe { (feature::arm64_isa_features & feature) != 0 }
}

/// Architectural interrupt frame for aarch64 user space entry/exit.
///
/// Layout must exactly match C++ `iframe_t` (`zircon/kernel/arch/arm64/include/arch/regs.h`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Iframe {
    pub r: [u64; 30], // x0-x29
    pub elr: u64,     // ELR_EL1
    pub spsr: u64,    // SPSR_EL1
    pub lr: u64,      // x30
    pub usp: u64,     // SP_EL0
}

pub type SyscallRegs = Iframe;

zr::static_assert!(core::mem::size_of::<Iframe>() == 272);
zr::static_assert!(core::mem::align_of::<Iframe>() == 8);
zr::static_assert!(core::mem::offset_of!(Iframe, r) == 0);
zr::static_assert!(core::mem::offset_of!(Iframe, elr) == 240);
zr::static_assert!(core::mem::offset_of!(Iframe, spsr) == 248);
zr::static_assert!(core::mem::offset_of!(Iframe, lr) == 256);
zr::static_assert!(core::mem::offset_of!(Iframe, usp) == 264);

#[allow(unused_imports)]
pub use arch_types_bindings::{GeneralRegsSource, UserEntryState};

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_BASE: usize = 0xffff000000000000;
zr::static_assert!(KERNEL_ASPACE_BASE == aspace_bindings::KERNEL_ASPACE_BASE as usize);

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_SIZE: usize = 0x0001000000000000;
zr::static_assert!(KERNEL_ASPACE_SIZE == aspace_bindings::KERNEL_ASPACE_SIZE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_BASE: usize = 0x0000000000200000;
zr::static_assert!(USER_ASPACE_BASE == aspace_bindings::USER_ASPACE_BASE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_SIZE: usize = 0xffffff000000 - USER_ASPACE_BASE;
zr::static_assert!(USER_ASPACE_SIZE == aspace_bindings::USER_ASPACE_SIZE as usize);

/// Size of the restricted mode address space in unified address spaces.
/// We set the top of the restricted aspace to exactly halfway through the top
/// level page table.
pub const USER_RESTRICTED_ASPACE_SIZE: usize = (1usize << 47) - USER_ASPACE_BASE;
zr::static_assert!(
    USER_RESTRICTED_ASPACE_SIZE == aspace_bindings::USER_RESTRICTED_ASPACE_SIZE as usize
);

/// See ARM DDI 0487B.b, Table D4-25 for the maximum IPA range that can be used.
/// This size is based on a 4KB granule and a starting level of 1. We chose this
/// size due to the 40-bit physical address range on Cortex-A53.
pub const MMU_GUEST_SIZE_SHIFT: usize = aspace_bindings::MMU_GUEST_SIZE_SHIFT as usize;

// Submodule names the rest of the kernel resolves directly under `arch::arm64`: the
// restricted-mode part of the arch API contract in //zircon/kernel/arch/src/api.rs.
pub use restricted::{
    ArchSavedNormalState, dump, enter_full, enter_restricted,
    redirect_restricted_exception_to_normal, save_restricted_exception_state,
    save_restricted_iframe_state, save_restricted_syscall_state, save_state_pre_restricted_entry,
    validate_state_pre_restricted_entry,
};

#[cfg(ktest)]
/// Architecture unit tests for arm64.
#[unittest::suite(name = "arm64")]
mod arm64_tests {
    use super::vm::{is_kernel_address, is_user_accessible, is_valid_user_pc};
    use unittest::{assert_false, assert_true};

    /// Tests `periph_paddr_to_vaddr`.
    #[test]
    fn test_periph_paddr_to_vaddr() {
        assert_true!(
            super::periph_paddr_to_vaddr(crate::kernel::types::PAddr::from(u64::MAX)).is_none()
        );
    }

    /// Tests `is_user_accessible`.
    #[test]
    fn test_is_user_accessible() {
        assert_true!(is_user_accessible(0x0000_7fff_ffff_ffff));
        assert_false!(is_user_accessible(0x0080_0000_0000_0000));
        assert_false!(is_user_accessible(0xffff_ffff_8000_0000));
    }

    /// Tests `is_kernel_address`.
    #[test]
    fn test_is_kernel_address() {
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE));
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE + 0x1000));
        assert_true!(is_kernel_address(usize::MAX));
        assert_false!(is_kernel_address(0));
        assert_false!(is_kernel_address(0x1000));
        assert_false!(is_kernel_address(0x0000_7fff_ffff_ffff));
        assert_false!(is_kernel_address(KERNEL_ASPACE_BASE - 1));
    }

    /// Tests `is_valid_user_pc`.
    #[test]
    fn test_is_valid_user_pc() {
        // Null pointer is valid (used for threads intended to fault).
        assert_true!(is_valid_user_pc(0));
        // Valid userspace addresses.
        assert_true!(is_valid_user_pc(0x1000));
        assert_true!(is_valid_user_pc(0x0000_7fff_ffff_0000));
        // Inaccessible user address (bit 55 set).
        assert_false!(is_valid_user_pc(0x0080_0000_0000_0000));
        // Kernel address.
        assert_false!(is_valid_user_pc(KERNEL_ASPACE_BASE));
        assert_false!(is_valid_user_pc(0xffff_ffff_8000_0000));
    }
}

#[cfg(ktest)]
/// ARM clock unit tests.
#[unittest::suite(name = "arm_clock_rust")]
mod arm_clock_tests {
    use crate::arch_rs::InterruptDisableGuard;
    use crate::kernel::mp::{get_online_mask, is_cpu_online};
    use crate::kernel::percpu::PerCpu;
    use crate::kernel::scheduler::{peek_active_mask, peek_is_active};
    use crate::kernel::thread::{DEFAULT_PRIORITY, ThreadPtr, create_with_priority};
    use crate::kernel::types::{SMP_MAX_CPUS, cpu_num_to_mask};
    use crate::platform_rs::timer::InstantMono;
    use boot_options::BootOptions;
    use core::sync::atomic::{AtomicU32, Ordering};
    use kprint::kprintln;
    use unittest::unwrap_ok;

    struct Args {
        waiting: AtomicU32,
    }

    extern "C" fn event_stream_worker(args_: *mut core::ffi::c_void) -> i32 {
        // SAFETY: `args_` points to `Args` on the caller's stack, valid until all threads join.
        let args = unsafe { &*(args_ as *const Args) };
        {
            let _guard = InterruptDisableGuard::new();

            // Signal that we are ready.
            args.waiting.fetch_sub(1, Ordering::SeqCst);
            // Wait until everyone else is ready.
            while args.waiting.load(Ordering::SeqCst) > 0 {
                core::hint::spin_loop();
            }

            // If the event stream is working, it (or something else) will break us out on each
            // iteration.
            for _ in 0..1000 {
                // The SEVL sets the event flag for this CPU.  The first WFE consumes the now set
                // event flag.  By setting then consuming, we can be sure the second WFE will
                // actually wait for an event.
                // SAFETY: `sevl;wfe;wfe` sets the local event flag and waits for an event.
                unsafe {
                    core::arch::asm!("sevl;wfe;wfe");
                }
            }
        }
        kprintln!("cpu-{} done", crate::arch_rs::curr_cpu_num());
        0
    }

    // Verify that the event stream will break CPUs out of WFE.
    //
    // Start one thread for each CPU that's online and active.  Each thread will then disable
    // interrupts and issue a series of WFEs.  If the event stream is working as expected, each
    // thread will eventually complete its series of WFEs and terminate.  If the event stream is not
    // working as expected, one or more threads will hang.
    /// Event Stream
    #[test]
    fn test_event_stream() {
        if !BootOptions::get().arm64_event_stream_enabled {
            kprintln!("event stream disabled, skipping test");
            return true;
        }

        let args = Args { waiting: AtomicU32::new(0) };
        let mut threads: [Option<ThreadPtr>; SMP_MAX_CPUS] = [None; SMP_MAX_CPUS];

        // How many online+active CPUs do we have?
        let num_cpus = (get_online_mask() & peek_active_mask()).count_ones();
        args.waiting.store(num_cpus, Ordering::SeqCst);

        // Create a thread bound to each online+active CPU, but don't start them just yet.
        let mut last: cpu_num_t = 0;
        let count = PerCpu::processor_count() as cpu_num_t;
        for i in 0..count {
            if is_cpu_online(i) && peek_is_active(i) {
                // SAFETY: `event_stream_worker` and `args` remain valid until `join` completes.
                let t = unsafe {
                    create_with_priority(
                        c"test_event_stream".as_ptr(),
                        event_stream_worker,
                        core::ptr::from_ref(&args).cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )
                };
                let t_ptr = unwrap_ok!(t);
                // SAFETY: `t_ptr` was just created and has not been resumed yet.
                unsafe { t_ptr.set_cpu_affinity(cpu_num_to_mask(i)) };
                threads[i as usize] = Some(t_ptr);
                last = i;
            }
        }

        // Because these threads have hard affinity and will disable interrupts we need to take care
        // in how we start them.  If we start one that's bound to our current CPU, we may get
        // preempted deadlock.  To avoid this, bind the current thread to the *last* online+active
        // CPU.
        // SAFETY: `ThreadPtr::current()` returns the valid current thread.
        let orig_mask = unsafe {
            let curr = ThreadPtr::current();
            let orig = curr.get_cpu_affinity();
            curr.set_cpu_affinity(cpu_num_to_mask(last));
            orig
        };
        let _restore_mask = zr::defer(move || {
            // SAFETY: `ThreadPtr::current()` returns the valid current thread.
            unsafe { ThreadPtr::current().set_cpu_affinity(orig_mask) };
        });

        // Now that we're running on the last online+active CPU we can simply start them in order.
        for t_ptr in threads.iter().take(PerCpu::processor_count()).flatten() {
            // SAFETY: `t_ptr` is a valid unresumed thread.
            unsafe { t_ptr.resume() };
        }

        // Finally, wait for them to complete.
        for t_ptr in threads.iter().take(PerCpu::processor_count()).flatten() {
            // SAFETY: `t_ptr` is a valid thread that has not been joined or detached.
            let _ = unsafe { t_ptr.join(InstantMono::INFINITE) };
        }
    }
}
