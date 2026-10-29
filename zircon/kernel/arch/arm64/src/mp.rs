// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 multiprocessor support: CPU topology, IPIs, CPU hotplug and per-CPU state (reached
//! through the fixed x20 register).

use super::feature::{MPIDR_EL1_RAW, midr_to_microarch};
use crate::kernel::mp::{MpIpi, MpIpiTarget, MpUnplugEvent, SMP_MAX_CPUS};
use crate::kernel::types::{INVALID_CPU, cpu_mask_t, cpu_num_t, mask_all_but_one};
use arm64_feature_bindings::arm64_microarch;
use arm64_mp_bindings::{ARM64_MPIDR_MASK, PERCPU_IN_RESTRICTED_MODE};
use core::arch::asm;
use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicU64, Ordering};
use debug::ltracef;
use libarch::arm64::MIDR_EL1;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    fn cpp_platform_halt_cpu();
    fn platform_start_cpu(cpu_id: cpu_num_t, mpid: u64, context: u64) -> Result<(), Status>;
    fn arm64_create_secondary_stack(cpu_num: cpu_num_t, out: *mut usize) -> Result<(), Status>;
    fn arm64_free_secondary_stack(cpu_num: cpu_num_t) -> Result<(), Status>;
}

/// Per cpu structure, pointed to by a fixed register while in kernel mode.
/// Aligned on the maximum architectural cache line to avoid cache
/// line sharing between cpus.
pub(super) type Arm64Percpu = arm64_mp_bindings::arm64_percpu;

zr::static_assert!(size_of::<Arm64Percpu>() == 64);
zr::static_assert!(align_of::<Arm64Percpu>() == super::MAX_CACHE_LINE);
zr::static_assert!(offset_of!(Arm64Percpu, cpu_num) == 0);
zr::static_assert!(offset_of!(Arm64Percpu, blocking_disallowed) == 4);
zr::static_assert!(offset_of!(Arm64Percpu, num_spinlocks) == 8);
zr::static_assert!(offset_of!(Arm64Percpu, microarch) == 12);
zr::static_assert!(offset_of!(Arm64Percpu, should_invalidate_bp_on_context_switch) == 16);
zr::static_assert!(offset_of!(Arm64Percpu, should_invalidate_bp_on_el0_exception) == 17);
zr::static_assert!(offset_of!(Arm64Percpu, high_level_percpu) == 24);
zr::static_assert!(
    offset_of!(Arm64Percpu, in_restricted_mode) == PERCPU_IN_RESTRICTED_MODE as usize
);

// Use the x20 register to always point at the local cpu structure for fast access.
// x20 is the first available callee-saved register that clang will allow to be marked
// as fixed (via -ffixed-x20 command line, +reserve-x20 for Rust). Since it's callee saved
// when making firmware calls to PSCI or SMCC the register will be naturally saved and restored.

/// Points x20 at `percpu`, the calling CPU's `arm64_percpu`.
///
/// # Safety
///
/// `percpu` must point at the calling CPU's element of `ARM64_PERCPU_ARRAY`. C++, assembly and
/// `read_percpu_field!` / `write_percpu_field!` dereference x20 from then on.
#[inline(always)]
unsafe fn arm64_write_percpu_ptr(percpu: *mut Arm64Percpu) {
    // SAFETY: x20 is reserved (`+reserve-x20`), so the compiler never allocates it and this write
    // clobbers nothing it tracks. The caller guarantees that `percpu` is the calling CPU's element.
    // The block is not `nomem`, so the compiler keeps it in order with the stores that set up the
    // element.
    unsafe {
        asm!("mov x20, {percpu}", percpu = in(reg) percpu, options(nostack, preserves_flags));
    }
}

/// Returns x20, the calling CPU's `arm64_percpu` once `arm64_init_percpu_early()` has run on it.
#[inline(always)]
pub(super) fn arm64_read_percpu_ptr() -> *mut Arm64Percpu {
    let percpu: *mut Arm64Percpu;
    // SAFETY: Copying the reserved x20 into an output register reads no memory and clobbers
    // nothing. The block is not `pure`, so each call reads x20 afresh.
    unsafe {
        asm!("mov {percpu}, x20", percpu = lateout(reg) percpu, options(nostack, preserves_flags));
    }
    percpu
}

/// [`arm64_read_percpu_ptr`] as the pointer type that `arch_thread::current_percpu_ptr` has.
#[inline(always)]
pub(super) fn arm64_read_percpu_ptr_for_thread() -> *mut arm64_thread_bindings::arm64_percpu {
    // Both bindings crates' `arm64_percpu` name the same C++ `struct arm64_percpu`.
    arm64_read_percpu_ptr().cast()
}

/// A type that an `arm64_percpu` field has, with the load and store instruction of its width.
///
/// # Safety
///
/// `read` and `write` must each be a single load or store of `size_of::<Self>()` bytes at
/// `x20 + OFFSET`, in an `asm!` block with none of the `pure`, `nomem` or `readonly` options (see
/// below).
pub(super) unsafe trait PercpuField: Copy {
    /// Loads the field at `OFFSET` of the calling CPU's `arm64_percpu`.
    ///
    /// # Safety
    ///
    /// `OFFSET` must be the offset of a field of type `Self` in `Arm64Percpu`, and x20 must point
    /// at the calling CPU's `arm64_percpu`, as it does once `arm64_init_percpu_early()` has run on
    /// that CPU.
    unsafe fn read<const OFFSET: usize>() -> Self;

    /// Stores `value` to the field at `OFFSET` of the calling CPU's `arm64_percpu`.
    ///
    /// # Safety
    ///
    /// As for `read`. Also, `value` must be valid for the field: C++ and assembly read these fields
    /// and rely on what they hold. No Rust reference to the field may be live.
    unsafe fn write<const OFFSET: usize>(value: Self);
}

// None of the loads and stores below is `pure`, to force a read of the field to
// make sure the compiler always emits a read when asked and does not cache a copy
// between.  For the same reason, this can't be done via a copy of the percpu
// pointer, since the compiler could copy x20 into another register and access it
// after a reschedule.  None is `nomem` or `readonly` either, so the compiler treats
// each as touching memory and keeps it in order with the spinlock acquire and
// release around it, as clang does for the C++ `__asm__ volatile` accessors.

// SAFETY: Each method is one `ldr w`/`str w` at `x20 + OFFSET`, and its `asm!` options are only
// `nostack` and `preserves_flags`, so it is neither `pure`, `nomem` nor `readonly`.
unsafe impl PercpuField for u32 {
    #[inline(always)]
    unsafe fn read<const OFFSET: usize>() -> Self {
        let value: u32;
        // SAFETY: The caller guarantees that x20 points at the calling CPU's `arm64_percpu` (it is
        // reloaded on exception entry and never allocated by the compiler) and that a `u32` field
        // is at `OFFSET`.
        unsafe {
            asm!(
                "ldr {value:w}, [x20, #{offset}]",
                value = lateout(reg) value,
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
        value
    }

    #[inline(always)]
    unsafe fn write<const OFFSET: usize>(value: Self) {
        // SAFETY: As in `read`, the store hits the calling CPU's `u32` field at `OFFSET`. The
        // caller guarantees that `value` is valid for that field and that no Rust reference to it
        // is live.
        unsafe {
            asm!(
                "str {value:w}, [x20, #{offset}]",
                value = in(reg) value,
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
    }
}

// The `ldr` instruction is 64 bits or 32 bits depending on the register name
// used.  All the load instructions for sizes smaller than 64 bits use the
// 32-bit register form, but sizes smaller than 32 bits have their own load
// instructions: `ldrh` for 16 bits, `ldrb` for 8 bits.  These still write a
// full 32-bit register, so the output operand is u32.  Narrow the result to
// the actual value type.
//
// The `str` instruction is 64 bits or 32 bits depending on the register name
// used.  All the store instructions for sizes smaller than 64 bits use the
// 32-bit register form, but sizes smaller than 32 bits have their own store
// instructions: `strh` for 16 bits, `strb` for 8 bits.  In all cases the
// "unused" bits of the register are ignored.  But just in case, zero-extend
// the value from its actual bit width up to u32 since the asm operand
// uses u32 to reflect that the 32-bit operand register appears in asm.

// SAFETY: Each method is one `ldrb`/`strb` at `x20 + OFFSET`, and its `asm!` options are only
// `nostack` and `preserves_flags`, so it is neither `pure`, `nomem` nor `readonly`.
unsafe impl PercpuField for bool {
    #[inline(always)]
    unsafe fn read<const OFFSET: usize>() -> Self {
        let value: u32;
        // SAFETY: As in the `u32` impl, x20 points at the calling CPU's `arm64_percpu`, and the
        // caller guarantees that a `bool` field is at `OFFSET`.
        unsafe {
            asm!(
                "ldrb {value:w}, [x20, #{offset}]",
                value = lateout(reg) value,
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
        // Comparing rather than reinterpreting the byte yields a valid `bool` whatever the field
        // holds.
        value != 0
    }

    #[inline(always)]
    unsafe fn write<const OFFSET: usize>(value: Self) {
        // SAFETY: As in `read`, the store hits the calling CPU's `bool` field at `OFFSET`. The
        // caller guarantees that no Rust reference to it is live; 0 and 1 are both valid for it.
        unsafe {
            asm!(
                "strb {value:w}, [x20, #{offset}]",
                value = in(reg) u32::from(value),
                offset = const OFFSET,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Reads the field of type `T` at `OFFSET` of the calling CPU's `arm64_percpu`.
/// `read_percpu_field!` derives both from a field name.
///
/// # Safety
///
/// `OFFSET` must be the offset of the field that `_field` projects to, and x20 must point at the
/// calling CPU's `arm64_percpu` (see `PercpuField::read`).
#[inline(always)]
pub(super) unsafe fn arm64_read_percpu_field<T: PercpuField, const OFFSET: usize>(
    _field: fn(&Arm64Percpu) -> &T,
) -> T {
    const { assert!((OFFSET & (align_of::<T>() - 1)) == 0, "Bad offset alignment") };
    // SAFETY: The caller guarantees that `OFFSET` is the offset of a `T` field and that x20 points
    // at the calling CPU's `arm64_percpu`.
    unsafe { T::read::<OFFSET>() }
}

/// Writes `value` to the field of type `T` at `OFFSET` of the calling CPU's `arm64_percpu`.
/// `write_percpu_field!` derives both from a field name.
///
/// # Safety
///
/// `OFFSET` must be the offset of the field that `_field` projects to, and x20 must point at the
/// calling CPU's `arm64_percpu` (see `PercpuField::read`). `value` must be valid for that field:
/// C++ and assembly read it and rely on what it holds (C++ indexes per-CPU arrays with `cpu_num`,
/// for example). No Rust reference to the field may be live.
#[inline(always)]
pub(super) unsafe fn arm64_write_percpu_field<T: PercpuField, const OFFSET: usize>(
    _field: fn(&Arm64Percpu) -> &T,
    value: T,
) {
    const { assert!((OFFSET & (align_of::<T>() - 1)) == 0, "Bad offset alignment") };
    // SAFETY: The caller guarantees that `OFFSET` is the offset of a `T` field, that x20 points at
    // the calling CPU's `arm64_percpu`, that `value` is valid for the field and that no reference
    // to the field is live.
    unsafe { T::write::<OFFSET>(value) }
}

/// Reads `field` of the calling CPU's `arm64_percpu` with one load through x20, in the width of
/// the field's type.
macro_rules! read_percpu_field {
    ($field:ident) => {
        // SAFETY: The offset and the projection both name `$field`. x20 points at the calling CPU's
        // `arm64_percpu`, the same assumption C++'s READ_PERCPU_FIELD makes.
        unsafe {
            $crate::arch_rs::arm64::mp::arm64_read_percpu_field::<
                _,
                { core::mem::offset_of!($crate::arch_rs::arm64::mp::Arm64Percpu, $field) },
            >(|percpu| &percpu.$field)
        }
    };
}
pub(super) use read_percpu_field;

/// Writes `value` to `field` of the calling CPU's `arm64_percpu` with one store through x20, in
/// the width of the field's type.
///
/// Expands to a call of the `unsafe fn` `arm64_write_percpu_field`, whose offset and projection
/// both name `field`. The caller's `unsafe` block answers for the rest of its contract: that x20
/// points at the calling CPU's `arm64_percpu`, that `value` is valid for `field`, and that no Rust
/// reference to `field` is live. Rust code reaches `arm64_percpu` fields only through these
/// macros, which form no reference to one.
macro_rules! write_percpu_field {
    ($field:ident, $value:expr) => {
        $crate::arch_rs::arm64::mp::arm64_write_percpu_field::<
            _,
            { core::mem::offset_of!($crate::arch_rs::arm64::mp::Arm64Percpu, $field) },
        >(|percpu| &percpu.$field, $value)
    };
}
pub(super) use write_percpu_field;

/// Returns the number of the calling CPU.
#[inline(always)]
pub(super) fn arch_curr_cpu_num() -> cpu_num_t {
    read_percpu_field!(cpu_num)
}

/// Sets the calling CPU's flag that tracks whether it is in restricted mode.
///
/// x20 points at the calling CPU's `arm64_percpu` from that CPU's first
/// `arm64_init_percpu_early()` on, which runs before any thread does.
#[inline(always)]
pub(super) fn arch_set_restricted_flag(restricted: bool) {
    // SAFETY: x20 points at the calling CPU's `arm64_percpu`, the same assumption C++'s
    // WRITE_PERCPU_FIELD makes. `exceptions.S` reads `in_restricted_mode` as zero or nonzero, and
    // the value is 0 or 1. Rust forms no reference to the field.
    unsafe { write_percpu_field!(in_restricted_mode, u32::from(restricted)) };
}

/// Mask the MPIDR register to only leave the AFFx ids.
const MPID_AFF_MASK: u64 = ARM64_MPIDR_MASK;

/// A list of mpids, indexed by cpu_id.
/// We can leave this zero-initialized as MPID 0 can be a valid value for CPU 0.
///
/// Atomic because the boot CPU registers the mpids of later CPUs while CPUs it has already started
/// scan the list in `arm64_mpidr_to_cpu_num`.
static ARM64_CPU_LIST: [AtomicU64; SMP_MAX_CPUS] = [const { AtomicU64::new(0) }; SMP_MAX_CPUS];

/// Per cpu structures, each cpu will point to theirs using the fixed register.
///
/// Rust reaches the elements only through place expressions and raw pointers, never a reference:
/// each CPU writes its own element through x20 (C++, assembly and `write_percpu_field!`) while
/// other code here reads and writes fields of any element by index.
//
// SAFETY: Every field of `arm64_percpu` is an integer, a `bool` or a raw pointer, for which all
// zero bytes are a valid value.
static mut ARM64_PERCPU_ARRAY: [Arm64Percpu; SMP_MAX_CPUS] = unsafe { core::mem::zeroed() };

/// Total number of detected cpus.
///
/// C++ sets it through `arch_set_num_cpus()` in `<arch/arm64/mp.h>`, once on the boot CPU before
/// the secondary CPUs start, and reads it through `arch_max_num_cpus()`.
#[unsafe(no_mangle)]
pub static mut arm_num_cpus: u32 = 1;

/// Returns the number of detected cpus.
#[inline(always)]
fn arch_max_num_cpus() -> u32 {
    // SAFETY: The boot CPU writes `arm_num_cpus` once, through `arch_set_num_cpus()`, before it
    // starts any secondary CPU, and nothing writes it afterwards.
    unsafe { arm_num_cpus }
}

/// Records `mpid` as the MPIDR of `cpu_id`, and `cpu_id` as that CPU's number.
///
/// # Safety
///
/// CPU `cpu_id` must not have started yet, or be the calling CPU: it reads its `cpu_num` through
/// x20, and C++ indexes per-CPU arrays with it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_register_mpid(cpu_id: u32, mpid: u64) {
    // SAFETY: Only this function writes `cpu_num`, and the caller guarantees that CPU `cpu_id`
    // is not running elsewhere, so nothing reads the field concurrently. `cpu_id` is the value
    // that CPU's per-CPU array indices are taken from.
    unsafe { ARM64_PERCPU_ARRAY[cpu_id as usize].cpu_num = cpu_id };
    ARM64_CPU_LIST[cpu_id as usize].store(mpid, Ordering::Relaxed);
}

/// Translate mpidr to cpu number.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_mpidr_to_cpu_num(mut mpidr: u64) -> cpu_num_t {
    mpidr &= MPID_AFF_MASK;
    for (i, entry) in ARM64_CPU_LIST.iter().enumerate().take(arch_max_num_cpus() as usize) {
        if entry.load(Ordering::Relaxed) == mpidr {
            return i as cpu_num_t;
        }
    }
    // Because early boot will register CPU 0's MPID, a matching CPU number should always be found.
    INVALID_CPU
}

/// Translate a CPU number back to the MPIDR of the CPU.
#[unsafe(no_mangle)]
pub extern "C" fn arch_cpu_num_to_mpidr(cpu_num: cpu_num_t) -> u64 {
    debug_assert!((cpu_num as usize) < ARM64_CPU_LIST.len());
    ARM64_CPU_LIST[cpu_num as usize].load(Ordering::Relaxed)
}

/// Reschedules tasks on the cpus specified by mask. Mask will not contain the local cpu_id.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_reschedule(mask: cpu_mask_t) {
    arch_mp_send_ipi(MpIpiTarget::Mask, mask, MpIpi::Reschedule);
}

/// Send inter processor interrupt, if supported.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_send_ipi(target: MpIpiTarget, mut mask: cpu_mask_t, ipi: MpIpi) {
    ltracef!("target {} mask {:#x}, ipi {}\n", target as i32, mask, ipi as i32);

    // translate the high level target + mask mechanism into just a mask
    match target {
        MpIpiTarget::All => {
            mask = ((1u64 << SMP_MAX_CPUS) - 1) as cpu_mask_t;
        }
        MpIpiTarget::AllButLocal => {
            mask = mask_all_but_one(arch_curr_cpu_num());
        }
        MpIpiTarget::Mask => {}
    }

    // SAFETY: `interrupt_send_ipi` requires the interrupt ops to be registered. The interrupt
    // controller driver registers them during platform early init, before the kernel sends IPIs.
    let _ = unsafe { crate::pdev_interrupt::interrupt_send_ipi(mask, ipi) };
}

/// Sets up the calling CPU's per-CPU structure and points x20 at it.
///
/// # Safety
///
/// The calling CPU's mpid must have been registered with `arch_register_mpid`, and the calling
/// CPU must not read or write per-CPU state through x20 before the first call on that CPU
/// returns. A later call rewrites the same x20 value and `microarch`. No call may overlap
/// `arch_quirks_needs_arm_erratum_858921_mitigation`, which reads every CPU's `microarch`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_init_percpu_early() {
    // slow lookup the current cpu id and setup the percpu structure
    let mpidr = MPIDR_EL1_RAW.read();
    let cpu = arm64_mpidr_to_cpu_num(mpidr);
    // SAFETY: `cpu` is the calling CPU's number, found from its own MPIDR, so the element is the
    // calling CPU's. The array is a static, so the pointer stays valid for as long as x20 holds it.
    unsafe { arm64_write_percpu_ptr(&raw mut ARM64_PERCPU_ARRAY[cpu as usize]) };

    // read the midr and set the microarch of this cpu in the percpu array
    let midr = (MIDR_EL1.read().bits() & 0xFFFF_FFFF) as u32;
    // SAFETY: The element is the calling CPU's, and only this function writes `microarch`, so no
    // other write races with this one. The only cross-CPU reader is
    // `arch_quirks_needs_arm_erratum_858921_mitigation`, which the caller does not run
    // meanwhile. The value is an `arm64_microarch` discriminant, which is what uarch and that
    // check read the field as.
    unsafe {
        ARM64_PERCPU_ARRAY[cpu as usize].microarch = midr_to_microarch(u64::from(midr)) as u32;
    }
}

/// Initializes the calling CPU's interrupt controller state.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_init_percpu() {
    // SAFETY: `interrupt_init_percpu` requires the interrupt ops to be registered. The interrupt
    // controller driver registers them during platform early init, before `arch_init` and before
    // any secondary CPU starts.
    unsafe { crate::pdev_interrupt::interrupt_init_percpu() };
}

/// Used to suspend work on a CPU until it is further shutdown.
/// This will only be invoked with interrupts disabled.  This function
/// must not re-enter the scheduler.
/// flush_done should be signaled after state is flushed.
///
/// # Safety
///
/// `flush_done` must point at a live `MpUnplugEvent` that stays alive until the CPU that waits on
/// it observes the signal. The generic mp layer owns it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_flush_state_and_halt(flush_done: *mut MpUnplugEvent) -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());
    crate::kernel::thread::preempt_disable();
    // SAFETY: The caller guarantees that `flush_done` points at a live `MpUnplugEvent`.
    unsafe { crate::kernel::mp::unplug_event_signal(flush_done) };
    // SAFETY: `cpp_platform_halt_cpu` takes no arguments and has no caller preconditions.
    unsafe { cpp_platform_halt_cpu() };
    panic!("control should never reach here");
}

/// Should be invoked by platform_mp_prep_cpu_unplug to ask the
/// arch to do whatever it needs to do to stop the CPU.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    if cpu_id == 0 || cpu_id >= arch_max_num_cpus() {
        return Err(Status::INVALID_ARGS);
    }
    Ok(())
}

/// Should be invoked by platform_mp_cpu_unplug to ask the
/// arch to do whatever it needs to do to stop the CPU.
#[unsafe(no_mangle)]
pub extern "C" fn arch_mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // we do not allow unplugging the bootstrap processor
    if cpu_id == 0 || cpu_id >= arch_max_num_cpus() {
        return Err(Status::INVALID_ARGS);
    }
    Ok(())
}

/// Should be invoked by platform_mp_cpu_hotplug to ask the arch
/// to bring a CPU up and enter it into the scheduler.
///
/// # Safety
///
/// The caller must serialize this with every other hotplug and unplug of `cpu_id`, and `cpu_id`
/// must be fully stopped: not still booting and not partway through halting. Otherwise its
/// secondary stack can be reinitialized or freed while that CPU is running on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_mp_cpu_hotplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // arm64_create_secondary_stack indexes its per-CPU threads by `cpu_id - 1`.
    if cpu_id == 0 || cpu_id >= arch_max_num_cpus() || cpu_id as usize >= SMP_MAX_CPUS {
        return Err(Status::INVALID_ARGS);
    }
    if crate::kernel::mp::is_cpu_online(cpu_id) {
        return Err(Status::BAD_STATE);
    }

    // Create a stack for the thread running on the CPU.
    let mut stack: usize = 0;
    // SAFETY: `stack` is a valid place for the stack pointer to be written to.
    unsafe { arm64_create_secondary_stack(cpu_id, &raw mut stack) }?;

    // Start the CPU.
    let mpid = ARM64_CPU_LIST[cpu_id as usize].load(Ordering::Relaxed);
    // SAFETY: `stack` is the stack `arm64_create_secondary_stack` just created for `cpu_id`, and
    // `mpid` is the MPIDR registered for it.
    let status = unsafe { platform_start_cpu(cpu_id, mpid, stack as u64) };
    if status.is_err() {
        // Start failed, so free the stack.
        // SAFETY: The CPU did not start, so nothing runs on the stack being freed.
        let free_stack_status = unsafe { arm64_free_secondary_stack(cpu_id) };
        debug_assert!(free_stack_status.is_ok());
    }
    status
}

/// If we have any A73 cores in the system, we need to work around
/// Cortex-A73 erratum 858921, described in:
///
/// <https://static.docs.arm.com/epm086451/120/Cortex-A73_MPCore_Software_Developers_Errata_Notice.pdf>
///
/// Right now, this means that we need a special version of current_mono_ticks() in
/// the kernel, and to inject a special version of zx_ticks_get in the VDSO if
/// our clients are not going to make a syscall in order to read the tick
/// counter.
///
/// # Safety
///
/// Do not call this function until all of the cores have been started and had a
/// chance to identify themselves.  Use mp_wait_for_all_cpus_ready
/// (kernel/mp.h) to establish this if needed. No CPU may be running
/// `arm64_init_percpu_early` meanwhile, as one does when it comes back up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_quirks_needs_arm_erratum_858921_mitigation() -> bool {
    // If there are any A73 cores in this system, then we need the clock read
    // mitigation.
    debug_assert!(SMP_MAX_CPUS >= arch_max_num_cpus() as usize);
    let num_cpus = arch_max_num_cpus().min(SMP_MAX_CPUS as u32);
    for i in 0..num_cpus {
        // SAFETY: Each CPU writes its `microarch` in its early init, before it identifies
        // itself. The caller has waited until every CPU has identified itself, and no CPU runs
        // that init meanwhile.
        if unsafe { ARM64_PERCPU_ARRAY[i as usize].microarch }
            == arm64_microarch::ARM_CORTEX_A73 as u32
        {
            return true;
        }
    }
    false
}

/// Setup the high-level percpu struct pointer for `cpu_num`.
///
/// # Safety
///
/// `percpu` must point at the high-level per-CPU structure of `cpu_num` and stay valid for the
/// kernel's lifetime, because `arch_get_curr_percpu()` dereferences it. The call must be made on
/// CPU `cpu_num`, or before that CPU starts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_setup_percpu(
    cpu_num: cpu_num_t,
    percpu: *mut arm64_mp_bindings::percpu,
) {
    // SAFETY: Taking the element's address reads nothing and forms no reference.
    let arch_percpu = unsafe { &raw mut ARM64_PERCPU_ARRAY[cpu_num as usize] };
    // SAFETY: `arch_percpu` points into the static array. Only this function writes
    // `high_level_percpu`, and the caller makes the call on CPU `cpu_num` (or before it starts),
    // so no other access to the field races with these.
    unsafe {
        debug_assert!(
            (*arch_percpu).high_level_percpu.is_null()
                || (*arch_percpu).high_level_percpu == percpu
        );
        (*arch_percpu).high_level_percpu = percpu;
    }
}
