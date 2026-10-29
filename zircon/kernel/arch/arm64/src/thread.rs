// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 thread state: initialization, context switching and user register state.
//!
//! The `arch_*` exports take `c_void` thread pointers, the shape `arch/src/api.rs` checks, and
//! cast each to the `Thread` that its C declaration names.

use core::arch::asm;
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::{with_exposed_provenance, with_exposed_provenance_mut};

use arm64_thread_bindings::{
    CURRENT_MDSCR_OFFSET, CURRENT_PERCPU_PTR_OFFSET, MSDCR_EL1_INITIAL_VALUE,
};
use debug::{dprintf, ltracef};
use regio::arm64::{SysReg, spec};

use super::fpu::{arm64_fpu_context_switch, arm64_fpu_restore_state, arm64_fpu_save_state};
use super::mp::{arch_set_restricted_flag, arm64_read_percpu_ptr_for_thread, read_percpu_field};
use super::registers::arm64_write_hw_debug_regs;
use super::uarch::arm64_uarch_do_spectre_v2_mitigation;
use super::{GeneralRegsSource, device_memory_barrier};
use crate::kernel::thread::{
    Thread, active_aspace, get_arch, get_shadow_call_base, get_stack_top, in_restricted,
    is_running, is_user_state_saved_locked,
};

const LOCAL_TRACE: u32 = 0;

/// Architecture-specific thread state (`struct arch_thread`).
type ArchThread = arm64_thread_bindings::arch_thread;

// `arch_thread` has `shadow_call_sp` only when the shadow call stack is on, as it is for kernel
// Rust. CURRENT_MDSCR_OFFSET is 56 only with that field, so bindgen's libclang saw it on too.
zr::static_assert!(cfg!(sanitize = "shadow-call-stack"));
zr::static_assert!(CURRENT_MDSCR_OFFSET == 56);

// The assembly offsets are relative to `thread_pointer_location`, where TPIDR_EL1 points.
zr::static_assert!(
    offset_of!(ArchThread, current_percpu_ptr) as isize
        - offset_of!(ArchThread, __bindgen_anon_1.thread_pointer_location) as isize
        == CURRENT_PERCPU_PTR_OFFSET as isize
);
zr::static_assert!(
    offset_of!(ArchThread, mdscr_el1) as isize
        - offset_of!(ArchThread, __bindgen_anon_1.thread_pointer_location) as isize
        == CURRENT_MDSCR_OFFSET as isize
);

/// Returns the calling CPU's current `stack_guard`, read where the compiler's stack protector reads
/// it: at its fixed offset from the thread pointer. This also works while the thread pointer still
/// points at early boot's thread-ABI region, which has the field at the same offset.
fn current_stack_guard() -> usize {
    let tp = TPIDR_EL1.read() as usize;
    let offset = offset_of!(ArchThread, stack_guard) as isize
        - offset_of!(ArchThread, __bindgen_anon_1.thread_pointer_location) as isize;
    // SAFETY: TPIDR_EL1 points at the thread pointer location of the current thread's
    // `arch_thread`, or of early boot's thread-ABI region, and both hold the guard at `offset`,
    // 8-byte aligned. Only the thread itself writes its guard once it has run.
    unsafe { with_exposed_provenance::<u8>(tp).offset(offset).cast::<usize>().read() }
}

/// Returns a pointer to the arm64 architectural state embedded in `thread`.
///
/// The pointer is only offset from `thread`, never read through, so this works on a thread that
/// is not yet fully constructed.
///
/// # Safety
///
/// `thread` points to a C++ `Thread`; the result is only an offset of `thread`.
#[inline(always)]
pub(super) unsafe fn thread_arch(thread: *mut Thread) -> *mut ArchThread {
    // SAFETY: the caller guarantees `thread`, and the facade only offsets the pointer.
    unsafe { get_arch(thread).cast() }
}

/// [`thread_arch`] for a `*const Thread`, whose arch state is only read.
///
/// # Safety
///
/// As for [`thread_arch`].
#[inline(always)]
pub(super) unsafe fn thread_arch_const(thread: *const Thread) -> *const ArchThread {
    // SAFETY: As for `thread_arch`, which only offsets the pointer: nothing is written through it.
    unsafe { thread_arch(thread.cast_mut()) }.cast_const()
}

/// Register state layout used by `arm64_context_switch()`.
#[repr(C)]
struct Arm64ContextSwitchFrame {
    r19: u64,
    zero: u64, // slot where x20 (percpu pointer) would be saved if it were
    r21: u64,
    r22: u64,
    r23: u64,
    r24: u64,
    r25: u64,
    r26: u64,
    r27: u64,
    r28: u64,
    r29: u64,
    lr: u64,
}

zr::static_assert!(size_of::<Arm64ContextSwitchFrame>() == 96);
zr::static_assert!(align_of::<Arm64ContextSwitchFrame>() == 8);
zr::static_assert!(offset_of!(Arm64ContextSwitchFrame, r29) == 80);
zr::static_assert!(offset_of!(Arm64ContextSwitchFrame, lr) == 88);

// assert that the context switch frame is a multiple of 16 to maintain
// stack alignment requirements per ABI
zr::static_assert!(size_of::<Arm64ContextSwitchFrame>().is_multiple_of(16));

pub(super) const TPIDR_EL0: SysReg<spec::TPIDR_EL0, u64> = SysReg::new();
const TPIDRRO_EL0: SysReg<spec::TPIDRRO_EL0, u64> = SysReg::new();
const TPIDR_EL1: SysReg<spec::TPIDR_EL1, u64> = SysReg::new();

unsafe extern "C" {
    /// Saves the callee-saved registers in a context switch frame on the current stack, stores
    /// the stack pointer to `*old_sp` and the shadow call stack pointer to `*old_scsp`, then
    /// switches to `new_sp`, `new_tpidr` and `new_scsp` and returns from the frame at `new_sp`.
    fn arm64_context_switch(
        old_sp: *mut usize,
        new_sp: usize,
        new_tpidr: usize,
        old_scsp: *mut *mut usize,
        new_scsp: *mut usize,
    );

    fn cpp_with_frame_pointers() -> bool;
}

/// Points TPIDR_EL1 at `t`'s `thread_pointer_location`, which makes `t` the current thread.
///
/// # Safety
///
/// `t` points to a `Thread` whose thread-ABI fields (`stack_guard`) are set, since code reads
/// them through the thread pointer from then on.
#[inline(always)]
unsafe fn arch_set_current_thread(t: *mut Thread) {
    // SAFETY: The caller guarantees `t`, and taking the field's address reads nothing.
    let tp = unsafe { &raw mut (*thread_arch(t)).__bindgen_anon_1.thread_pointer_location };
    // SAFETY: The caller guarantees that `t` is ready to be the current thread. The block is not
    // `nomem`, to prevent compiler reordering across the setting, ensuring all previous stores to
    // `t` (e.g., initialization) have completed before installation.
    unsafe { asm!("msr tpidr_el1, {tp}", tp = in(reg) tp, options(nostack, preserves_flags)) };
}

/// Initializes the architectural state of `thread` so that its first context switch enters
/// `entry_point` at the top of its stack.
///
/// # Safety
///
/// `thread` points to a `Thread` whose stack is allocated and to which the caller has exclusive
/// access: it holds the thread's lock, or owns the thread while constructing it. The thread is not
/// running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_thread_initialize(thread: *mut c_void, entry_point: usize) {
    let t: *mut Thread = thread.cast();
    // SAFETY: The caller guarantees `t`, and `thread_arch` only offsets the pointer.
    let arch = unsafe { thread_arch(t) };

    // zero out the entire arch state
    // SAFETY: `arch` points at `t`'s `arch_thread`, which nothing else accesses while the caller
    // has exclusive access to the not-running `t`. Every field is an integer, a `bool`, a raw
    // pointer or a union of those, for which all zero bytes are a valid value.
    unsafe { arch.write(core::mem::zeroed()) };

    // Set MDSCR to the default value.
    // SAFETY: As above.
    unsafe { (*arch).mdscr_el1 = u64::from(MSDCR_EL1_INITIAL_VALUE) };

    // create a default stack frame on the stack
    // SAFETY: The caller guarantees `t`.
    let stack_top = unsafe { get_stack_top(t) };

    // make sure the top of the stack is 16 byte aligned for EABI compliance
    debug_assert!(stack_top.is_multiple_of(16));

    let mut frame = with_exposed_provenance_mut::<Arm64ContextSwitchFrame>(stack_top);
    // SAFETY: `stack_top` is the top of `t`'s allocated stack, and a frame fits below it.
    frame = unsafe { frame.sub(1) };

    // fill in the entry point
    // SAFETY: `frame` is in `t`'s stack, which nothing uses before `t` first runs.
    unsafe { (*frame).lr = entry_point as u64 };

    // This is really a global (boot-time) constant value.
    // But it's stored in each thread struct to satisfy the
    // compiler ABI (TPIDR_EL1 + ZX_TLS_STACK_GUARD_OFFSET).
    // SAFETY: `t`'s arch state is the caller's, as above.
    unsafe { (*arch).stack_guard = current_stack_guard() };

    // set the stack pointer
    // SAFETY: As above.
    unsafe { (*arch).__bindgen_anon_1.sp = frame.expose_provenance() };
    // The shadow call stack grows up.
    // SAFETY: The caller guarantees `t`, and `t`'s arch state is the caller's, as above.
    unsafe {
        (*arch).shadow_call_sp = with_exposed_provenance_mut(get_shadow_call_base(t));
    }
}

/// Moves the calling CPU from the fake early-boot thread to `thread`, the first real thread on
/// the CPU.
///
/// # Safety
///
/// `thread` points to the `Thread` just constructed for the calling CPU, and the thread pointer
/// still points at the fake `arch_thread` that early boot code set up. Nothing on the CPU may
/// consult the current thread before this returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_thread_construct_first(thread: *mut c_void) {
    let t: *mut Thread = thread.cast();

    // Propagate the values from the fake arch_thread that the thread
    // pointer points to now (set up by physboot) into the real thread
    // structure being set up now.

    // Cache a pointer to the arch part of the thread being set up.
    // SAFETY: The caller guarantees `t`.
    let arch = unsafe { thread_arch(t) };
    // SAFETY: `t` is not yet visible to any other code.
    unsafe { (*arch).stack_guard = current_stack_guard() };

    // make sure the thread saves a copy of the current cpu pointer
    // SAFETY: As above, `t` is not yet visible to any other code.
    unsafe { (*arch).current_percpu_ptr = arm64_read_percpu_ptr_for_thread() };

    // Force the thread pointer immediately to the real struct.  If this
    // function's epilogue checks a stack canary it should read the same guard
    // value from either thread pointer.
    // SAFETY: `t`'s `stack_guard` was copied from the fake thread above.
    unsafe { arch_set_current_thread(t) };
}

/// Saves the user-mode TPIDR_EL0 and TPIDRRO_EL0 into `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds, and its user-mode thread
/// pointers are the ones in the registers.
#[inline(always)]
unsafe fn arm64_tpidr_save_state(thread: *mut Thread) {
    // SAFETY: The caller guarantees `thread`.
    let arch = unsafe { thread_arch(thread) };
    // SAFETY: The caller holds `thread`'s lock, so nothing else accesses its arch state.
    unsafe { (*arch).tpidr_el0 = TPIDR_EL0.read() };
    // SAFETY: As above.
    unsafe { (*arch).tpidrro_el0 = TPIDRRO_EL0.read() };
}

/// Loads `thread`'s saved user-mode TPIDR_EL0 and TPIDRRO_EL0 into the registers.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds.
#[inline(always)]
unsafe fn arm64_tpidr_restore_state(thread: *mut Thread) {
    // SAFETY: The caller guarantees `thread`.
    let arch = unsafe { thread_arch_const(thread) };
    // SAFETY: The caller holds `thread`'s lock, so nothing writes its arch state.
    TPIDR_EL0.write(unsafe { (*arch).tpidr_el0 });
    // SAFETY: As above.
    TPIDRRO_EL0.write(unsafe { (*arch).tpidrro_el0 });
}

/// Loads `thread`'s debug state into the hardware debug registers if it tracks one.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds.
#[inline(always)]
unsafe fn arm64_debug_restore_state(thread: *mut Thread) {
    // SAFETY: The caller guarantees `thread` and holds its lock, so nothing writes its arch state.
    let arch = unsafe { thread_arch_const(thread) };
    // If the thread has debug state, then install it, replacing the current contents.
    // SAFETY: As above.
    if unsafe { (*arch).track_debug_state } {
        core::hint::cold_path();
        // SAFETY: As above, the reference to `debug_state` sees no writer while it lives. Only
        // `arch_set_debug_regs` sets `track_debug_state`, and it stores beside it a state that
        // passed `arm64_validate_debug_state`. Nothing else writes the breakpoint and watchpoint
        // fields once the thread is initialized, where they are zeroed.
        unsafe { arm64_write_hw_debug_regs(&(*arch).debug_state) };
    }
}

/// Flushes the indirect branch predictors before the switch from `oldthread` to `newthread`,
/// when the calling CPU needs it.
///
/// # Safety
///
/// `oldthread` and `newthread` point to live `Thread`s whose locks the caller holds.
#[inline(always)]
unsafe fn arm64_context_switch_spec_mitigations(oldthread: *mut Thread, newthread: *mut Thread) {
    // Spectre V2: Flush Indirect Branch Predictor State, if:
    // 0) Speculative Execution infoleak mitigations are enabled AND
    // 1) The current CPU requires Spectre V2 mitigations AND
    // 2a) We are switching between threads in different address spaces AND
    // 2b)    the old address space is not nullptr (not a kernel thread).
    //        If the old thread is a kernel thread, it can be trusted not to attack userspace
    if read_percpu_field!(should_invalidate_bp_on_context_switch)
        // SAFETY: The caller guarantees both threads and holds their locks.
        && unsafe {
            (active_aspace(oldthread) != active_aspace(newthread))
                && !active_aspace(oldthread).is_null()
        }
    {
        arm64_uarch_do_spectre_v2_mitigation();
    }
}

/// Switches the calling CPU from `oldthread`, the current thread, to `newthread`, and returns
/// when another CPU or this one switches back to `oldthread`.
///
/// # Safety
///
/// `oldthread` is the current thread and `newthread` a thread ready to run. Both point to live
/// `Thread`s whose locks the caller holds. Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_context_switch(oldthread: *mut c_void, newthread: *mut c_void) {
    let oldthread: *mut Thread = oldthread.cast();
    let newthread: *mut Thread = newthread.cast();
    ltracef!(
        "old {:p} ({}), new {:p} ({})\n",
        oldthread,
        // SAFETY: `oldthread` points to a live `Thread`, per the caller's contract.
        unsafe { Thread::name_copy_ptr(oldthread) },
        newthread,
        // SAFETY: As above, for `newthread`.
        unsafe { Thread::name_copy_ptr(newthread) }
    );

    // DSB here to make sure any pending TLB or cache operations that we may be
    // preempting are complete before the thread switch. This avoids a problem in
    // case the thread is moved to a new cpu.
    device_memory_barrier();

    // Set the current cpu pointer in the new thread's structure so it can be
    // restored on exception entry.
    // SAFETY: The caller holds `newthread`'s lock, and `thread_arch` only offsets the pointer.
    unsafe { (*thread_arch(newthread)).current_percpu_ptr = arm64_read_percpu_ptr_for_thread() };

    // SAFETY: The caller holds `oldthread`'s lock.
    if unsafe { !is_user_state_saved_locked(oldthread) } {
        // SAFETY: The caller guarantees both threads and holds their locks. `oldthread` is
        // running, so the user registers hold its state.
        unsafe {
            arm64_fpu_context_switch(oldthread, newthread);
            arm64_tpidr_save_state(oldthread);
            arm64_tpidr_restore_state(newthread);
            // Not saving debug state because the arch_thread_t's debug state is authoritative.
            arm64_debug_restore_state(newthread);
        }
    } else {
        core::hint::cold_path();
        // Nothing left to save for |oldthread|, so just restore |newthread|.  Technically, we
        // could skip restoring here since we know a higher layer will restore before leaving the
        // kernel.  We restore anyway so we don't leave |oldthread|'s state lingering in the
        // hardware registers. The thinking is that:
        //
        // 1. The performance cost is tolerable - This code path is only executed by threads that
        // have taken a (zircon) exception or are being debugged so there should be no performance
        // impact to "normal" threads.
        //
        // 2. We want to avoid confusion - When, for example, the kernel panics and prints user
        // register state to a log, a future maintainer might be confused to find that some other
        // thread's user register state is present on a CPU that was executing an unrelated thread.
        //
        // 3. We want an extra layer of security - If we make a mistake and don't properly restore
        // the state before returning we might expose one thread's register state to another
        // thread.  By restoring early, that's less likely to happen (think belt and suspenders).
        // SAFETY: The caller guarantees `newthread` and holds its lock.
        unsafe {
            arm64_fpu_restore_state(newthread);
            arm64_tpidr_restore_state(newthread);
            arm64_debug_restore_state(newthread);
        }
    }

    // SAFETY: The caller guarantees `newthread`.
    arch_set_restricted_flag(unsafe { in_restricted(newthread) });

    // SAFETY: The caller guarantees both threads and holds their locks.
    unsafe { arm64_context_switch_spec_mitigations(oldthread, newthread) };

    // Call into the inner assembly context switch routine to save integer registers on the old
    // stack and swap to the new stack.
    // SAFETY: The caller holds both threads' locks, so their arch states are the caller's to use.
    // `oldthread` is running on this stack, and `newthread`'s `sp` and `shadow_call_sp` were saved
    // by its own last switch away or set by `arch_thread_initialize`. TPIDR_EL1 is pointed at
    // `newthread`'s `thread_pointer_location`, which makes it the current thread.
    unsafe {
        let old_arch = thread_arch(oldthread);
        let new_arch = thread_arch(newthread);
        arm64_context_switch(
            &raw mut (*old_arch).__bindgen_anon_1.sp,
            (*new_arch).__bindgen_anon_1.sp,
            (&raw const (*new_arch).__bindgen_anon_1.thread_pointer_location).expose_provenance(),
            &raw mut (*old_arch).shadow_call_sp,
            (*new_arch).shadow_call_sp,
        );
    }
}

/// Prints `thread`'s saved stack pointer if it is not running.
///
/// `Thread::Dump*DuringPanic` call this without the lock, as the C++ did; that read is best
/// effort.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds, shared or exclusive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_dump_thread(thread: *const c_void) {
    let t: *const Thread = thread.cast();
    // SAFETY: The caller guarantees `t` and holds its lock.
    if unsafe { !is_running(t) } {
        dprintf!(INFO, "\tarch: ");
        // SAFETY: As above, so nothing writes `t`'s saved `sp`.
        dprintf!(INFO, "sp {:#x}\n", unsafe { (*thread_arch_const(t)).__bindgen_anon_1.sp });
    }
}

/// Returns the frame pointer saved in `thread`'s context switch frame, or 0 without frame
/// pointers.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds and which is blocked, so its
/// `sp` points at the context switch frame its last switch away saved.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_thread_get_blocked_fp(thread: *mut c_void) -> usize {
    let t: *mut Thread = thread.cast();
    // SAFETY: `cpp_with_frame_pointers` takes no arguments and has no caller preconditions.
    if !unsafe { cpp_with_frame_pointers() } {
        return 0;
    }

    // SAFETY: The caller guarantees `t`.
    let frame = unsafe { arm64_get_context_switch_frame(t) };
    // SAFETY: The caller guarantees that `t` is blocked, so `frame` is the frame its switch away
    // saved, and the stack it lives on is not in use.
    unsafe { (*frame).r29 as usize }
}

/// Returns the context switch frame that `thread`'s saved `sp` points at.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds.
#[inline(always)]
unsafe fn arm64_get_context_switch_frame(thread: *mut Thread) -> *mut Arm64ContextSwitchFrame {
    // SAFETY: The caller guarantees `thread` and holds its lock, so nothing writes `sp`.
    with_exposed_provenance_mut(unsafe { (*thread_arch_const(thread)).__bindgen_anon_1.sp })
}

/// Saves the calling CPU's user-mode FPU state and thread pointers into `thread`.
///
/// # Safety
///
/// `thread` is the current thread, a live `Thread` whose lock the caller holds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_save_user_state(thread: *mut c_void) {
    let thread: *mut Thread = thread.cast();
    // SAFETY: The caller guarantees that `thread` is the current thread and holds its lock.
    unsafe {
        arm64_fpu_save_state(thread);
        arm64_tpidr_save_state(thread);
    }
    // Not saving debug state because the arch_thread_t's debug state is authoritative.
}

/// Loads `thread`'s saved debug state, user-mode FPU state and thread pointers into the
/// registers.
///
/// # Safety
///
/// `thread` is the current thread, a live `Thread` whose lock the caller holds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_restore_user_state(thread: *mut c_void) {
    let thread: *mut Thread = thread.cast();
    // SAFETY: The caller guarantees that `thread` is the current thread and holds its lock.
    unsafe {
        arm64_debug_restore_state(thread);
        arm64_fpu_restore_state(thread);
        arm64_tpidr_restore_state(thread);
    }
}

/// Records `iframe`, where `thread`'s user general registers are saved, for debugger access while
/// it is suspended or stopped in an exception.
///
/// # Safety
///
/// `thread` points to a live `Thread`, and the caller has exclusive access to its
/// `suspended_general_regs`. `iframe` stays valid until `arch_reset_suspended_general_regs`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_set_suspended_general_regs(
    thread: *mut c_void,
    source: GeneralRegsSource,
    iframe: *mut c_void,
) {
    let thread: *mut Thread = thread.cast();
    // SAFETY: The caller guarantees `thread`, and `thread_arch` only offsets the pointer.
    let arch = unsafe { thread_arch(thread) };
    // SAFETY: The caller has exclusive access to the field.
    debug_assert!(unsafe { (*arch).suspended_general_regs.is_null() });
    debug_assert!(!iframe.is_null());
    debug_assert!(source == GeneralRegsSource::Iframe, "invalid source {}\n", source as u32);
    // SAFETY: As above. The C++ declaration takes the `iframe_t` as a `void*`.
    unsafe { (*arch).suspended_general_regs = iframe.cast() };
}

/// Clears the record that `arch_set_suspended_general_regs` made.
///
/// # Safety
///
/// `thread` points to a live `Thread`, and the caller has exclusive access to its
/// `suspended_general_regs`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_reset_suspended_general_regs(thread: *mut c_void) {
    let thread: *mut Thread = thread.cast();
    // SAFETY: The caller guarantees `thread` and exclusive access to the field.
    unsafe { (*thread_arch(thread)).suspended_general_regs = core::ptr::null_mut() };
}
