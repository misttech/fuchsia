// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/interrupt/interrupt.cc

#[cfg(console_enabled)]
pub mod console;
#[cfg(not(console_enabled))]
use debug as _;

use crate::arch_rs::Iframe;
use crate::kernel::mp::MpIpi;
use crate::kernel::types::cpu_mask_t;
use core::cell::UnsafeCell;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_init::LazyInit;
use pin_init as _;
#[cfg(ktest)]
use unittest as _;
use zx_status::Status;

// NOTE: The C entry points below define functions declared in <dev/interrupt.h>
// (zircon/kernel/dev/interrupt/include/dev/interrupt.h); keep the two in sync. The interrupt
// controller interface itself is the `InterruptControllerOps` trait below; every registrant is
// Rust, so it has no C++ ABI.

pub use crate::dev_interrupt::{
    InterruptHandler, InterruptPolarity, InterruptTriggerMode, InterruptVector, MAX_INTERRUPTS,
    MsiBlock,
};

pub struct IntHandlerStruct {
    handler: InterruptHandler,
    permanent: AtomicBool,
}

// SAFETY: Concurrent mutation of `handler` is guarded by
// `PdevInterruptManager`'s lock. Lock-free reads are only performed when
// `permanent` is true, which is set during early boot and remains
// immutable for the remainder of the kernel's lifetime.
unsafe impl Sync for IntHandlerStruct {}

#[ksync::guarded]
pub struct PdevInterruptManager {
    #[guarded_by(lock)]
    table: [IntHandlerStruct; MAX_INTERRUPTS],

    #[mutex]
    lock: ksync::KMutex<ksync::RawSpinlock>,
}

// SAFETY: Internal table synchronization is managed via `lock` for dynamic
// registrations and atomic/immutable invariants for permanent handlers.
unsafe impl Sync for PdevInterruptManager {}

impl PdevInterruptManager {
    /// Returns a reference to an `IntHandlerStruct` slot without acquiring the lock.
    ///
    /// # Safety
    ///
    /// `vector` must be `< MAX_INTERRUPTS`. The caller must only access lock-free atomic fields
    /// (such as `permanent` or immutable `handler` once `permanent` is set to `true`).
    #[inline]
    pub unsafe fn slot_unchecked(&self, vector: usize) -> &IntHandlerStruct {
        let table_ptr = core::ptr::addr_of!(self.table) as *const IntHandlerStruct;
        // SAFETY: `self.table` is `repr(transparent)` around `UnsafeCell<[IntHandlerStruct; MAX_INTERRUPTS]>`.
        // `vector` is guaranteed to be < MAX_INTERRUPTS.
        unsafe { &*table_ptr.add(vector) }
    }
}

static PDEV_INTERRUPTS: LazyInit<PdevInterruptManager> = LazyInit::uninit();

/// Interrupt controller interface.
///
/// Implemented by the platform's interrupt controller driver and installed with
/// [`register_interrupt_controller`].
pub trait InterruptControllerOps: Sync + 'static {
    /// Disables (masks) the interrupt specified by `vector`. In the case of interrupts which
    /// target a specific CPU but share an interrupt vector number (for example, SGIs and PPIs in
    /// ARM's GIC interrupt controller designs), this affects the masked/unmasked state of the
    /// interrupt vector for the caller's CPU only. It does not mask the specified vector for any
    /// of the other CPUs.
    fn mask(&self, vector: InterruptVector) -> Result<(), Status>;

    /// Enables (unmasks) the interrupt specified by `vector`. In the case of interrupts which
    /// target a specific CPU but share an interrupt vector number (for example, SGIs and PPIs in
    /// ARM's GIC interrupt controller designs), this affects the masked/unmasked state of the
    /// interrupt vector for the caller's CPU only. It does not unmask the specified vector for
    /// any of the other CPUs.
    fn unmask(&self, vector: InterruptVector) -> Result<(), Status>;

    /// Deactivates the interrupt specified by `vector`.
    fn deactivate(&self, vector: InterruptVector) -> Result<(), Status>;

    /// Configures the specified interrupt vector. If it is invoked, it must be invoked prior to
    /// interrupt registration.
    fn configure(
        &self,
        vector: InterruptVector,
        tm: InterruptTriggerMode,
        pol: InterruptPolarity,
    ) -> Result<(), Status>;

    /// Returns the trigger mode and polarity configured for the specified interrupt vector.
    fn config(
        &self,
        vector: InterruptVector,
    ) -> Result<(InterruptTriggerMode, InterruptPolarity), Status>;

    /// Sets the affinity for an interrupt. Intrinsically set to cpu 0 by default.
    fn set_affinity(&self, vector: InterruptVector, mask: cpu_mask_t) -> Result<(), Status>;

    /// Returns `true` if `vector` is a valid interrupt for this controller.
    fn is_valid(&self, vector: InterruptVector, flags: u32) -> bool;

    /// Returns the base of the `[base, max]` range of vectors that can be used with `zx_interrupt`
    /// syscalls. This api will need to evolve if valid vector ranges later are not contiguous.
    fn base_vector(&self) -> InterruptVector;

    /// Returns the maximum of the `[base, max]` range of vectors that can be used with
    /// `zx_interrupt` syscalls. This api will need to evolve if valid vector ranges later are not
    /// contiguous.
    fn max_vector(&self) -> InterruptVector;

    /// Remaps the given interrupt vector.
    fn remap(&self, vector: InterruptVector) -> InterruptVector;

    /// Sends an inter-processor interrupt.
    fn send_ipi(&self, target: cpu_mask_t, ipi: MpIpi) -> Result<(), Status>;

    /// Performs early per-cpu initialization for the interrupt controller.
    fn init_percpu_early(&self);

    /// Performs per-cpu initialization for the interrupt controller.
    fn init_percpu(&self);

    /// Handles a platform interrupt taken on the calling CPU.
    fn handle_irq(&self, frame: &mut Iframe);

    /// Shuts down all interrupts.
    fn shutdown(&self);

    /// Shuts down interrupts for the calling CPU.
    ///
    /// Should be called before powering off the calling CPU.
    fn shutdown_cpu(&self);

    /// Suspends interrupts for the calling CPU.
    ///
    /// Should be called before entering a hardware suspend state.
    fn suspend_cpu(&self) -> Result<(), Status>;

    /// Resumes interrupts for the calling CPU.
    ///
    /// Should be called after coming out of a hardware suspend state.
    fn resume_cpu(&self) -> Result<(), Status>;

    /// Returns `true` if the platform supports MSI.
    ///
    /// If the platform supports MSI, it must supply valid implementations of
    /// [`Self::msi_alloc_block`], [`Self::msi_free_block`] and [`Self::msi_register_handler`].
    fn msi_is_supported(&self) -> bool;

    /// Returns `true` if the platform can mask individual MSI vectors at the platform level.
    ///
    /// If the platform supports MSI masking, it must supply a valid implementation of
    /// [`Self::msi_mask_unmask`].
    fn msi_supports_masking(&self) -> bool;

    /// Masks (`mask` is `true`) or unmasks (`mask` is `false`) the given MSI within `block`.
    fn msi_mask_unmask(&self, block: &MsiBlock, msi_id: u32, mask: bool);

    /// Platform allocation of blocks of MSI and MSI-X compatible IRQ targets.
    ///
    /// `requested_irqs` is the total number of irqs being requested. `can_target_64bit` is `true`
    /// if the target address of the MSI block can be located past the 4GB boundary, `false` if the
    /// target address must be in low memory. `is_msix` is `true` if this request is for an MSI-X
    /// compatible block, `false` for plain old MSI. `out_block` is the allocation bookkeeping to
    /// be filled out upon successful allocation of the requested block of IRQs.
    fn msi_alloc_block(
        &self,
        requested_irqs: u32,
        can_target_64bit: bool,
        is_msix: bool,
        out_block: &mut MsiBlock,
    ) -> Result<(), Status>;

    /// Frees a block of MSI IRQs previously allocated by [`Self::msi_alloc_block`]. This does not
    /// unregister IRQ handlers.
    fn msi_free_block(&self, block: &mut MsiBlock);

    /// Registers a handler function for a given `msi_id` within an MSI block. Passing an empty
    /// handler will effectively unregister a handler for a given `msi_id` within the block.
    fn msi_register_handler(&self, block: &MsiBlock, msi_id: u32, handler: InterruptHandler);

    /// Returns the `(pending, enabled)` state of the given interrupt vector.
    ///
    /// Only used by IRQ console commands.
    fn status(&self, vector: InterruptVector) -> Result<(bool, bool), Status> {
        let _ = vector;
        Err(Status::NOT_SUPPORTED)
    }
}

/// The interrupt controller used until a platform driver registers one.
///
/// By default most of these are empty stubs and the particular interrupt controller must override
/// all of them.
struct DefaultInterruptController;

impl InterruptControllerOps for DefaultInterruptController {
    fn mask(&self, _: InterruptVector) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn unmask(&self, _: InterruptVector) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn deactivate(&self, _: InterruptVector) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn configure(
        &self,
        _: InterruptVector,
        _: InterruptTriggerMode,
        _: InterruptPolarity,
    ) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn config(
        &self,
        _: InterruptVector,
    ) -> Result<(InterruptTriggerMode, InterruptPolarity), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn set_affinity(&self, _: InterruptVector, _: cpu_mask_t) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn is_valid(&self, _: InterruptVector, _: u32) -> bool {
        false
    }
    fn base_vector(&self) -> InterruptVector {
        InterruptVector(0)
    }
    fn max_vector(&self) -> InterruptVector {
        InterruptVector(0)
    }
    fn remap(&self, _: InterruptVector) -> InterruptVector {
        InterruptVector(0)
    }
    fn send_ipi(&self, _: cpu_mask_t, _: MpIpi) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn init_percpu_early(&self) {}
    fn init_percpu(&self) {}
    fn handle_irq(&self, _: &mut Iframe) {}
    fn shutdown(&self) {}
    fn shutdown_cpu(&self) {}
    fn suspend_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn resume_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn msi_is_supported(&self) -> bool {
        false
    }
    fn msi_supports_masking(&self) -> bool {
        false
    }
    fn msi_mask_unmask(&self, _: &MsiBlock, _: u32, _: bool) {}
    fn msi_alloc_block(&self, _: u32, _: bool, _: bool, _: &mut MsiBlock) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }
    fn msi_free_block(&self, _: &mut MsiBlock) {}
    fn msi_register_handler(&self, _: &MsiBlock, _: u32, _: InterruptHandler) {}
}

static DEFAULT_DRIVER: DefaultInterruptController = DefaultInterruptController;

/// Holds the active interrupt controller, `DEFAULT_DRIVER` until a driver registers.
///
/// It is not an `Option`: the entry points are inlined into every caller, and starting out at
/// `DEFAULT_DRIVER` keeps each call site to a load of the data and vtable pointers and the
/// indirect call, with no null check or fallback select.
struct DriverHolder(UnsafeCell<&'static dyn InterruptControllerOps>);

// SAFETY: The driver is written once during single-threaded early boot and read immutable
// thereafter.
unsafe impl Sync for DriverHolder {}

static ACTIVE_DRIVER: DriverHolder = DriverHolder(UnsafeCell::new(&DEFAULT_DRIVER));

/// Returns the registered interrupt controller, or the default no-op controller if no driver has
/// registered yet.
///
/// The `interrupt_init_percpu_early` init hook runs on secondary CPUs and may run before any
/// controller has registered, so this must never touch uninitialized state.
fn get_driver() -> &'static dyn InterruptControllerOps {
    // SAFETY: ACTIVE_DRIVER is written once during single-threaded early boot, and read immutable
    // thereafter.
    unsafe { *ACTIVE_DRIVER.0.get() }
}

/// Registers the platform's interrupt controller with the PDEV dispatch layer.
///
/// # Safety
///
/// Must be called at most once, during single-threaded boot: before secondary CPUs are started
/// and before anything can call `get_driver()` concurrently.
pub unsafe fn register_interrupt_controller(driver: &'static dyn InterruptControllerOps) {
    // SAFETY: This function's contract makes this the only initialization of `PDEV_INTERRUPTS`,
    // with no concurrent access.
    let _ = unsafe {
        Pin::static_ref(&PDEV_INTERRUPTS).init_pin(pin_init::pin_init!(PdevInterruptManager {
            table: [
                const {
                    IntHandlerStruct {
                        handler: InterruptHandler::DEFAULT,
                        permanent: AtomicBool::new(false),
                    }
                };
                MAX_INTERRUPTS
            ].into(),
            lock <- ksync::KSpinlock::init(),
        }))
    };

    // SAFETY: This function's contract makes this the only write to `ACTIVE_DRIVER`, with no
    // concurrent reader.
    unsafe {
        *ACTIVE_DRIVER.0.get() = driver;
    }
    // Architectural memory barrier to guarantee the vtable and data pointers, along with the
    // driver's early initialization writes, are visible to all future CPUs before they boot up and
    // enable interrupts.
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// Invokes the interrupt handler for the vector if it is present and permanent.
///
/// # Safety
///
/// `vector` must be less than `MAX_INTERRUPTS`.
pub unsafe fn pdev_invoke_int_if_present(vector: InterruptVector) -> bool {
    // SAFETY: vector is guaranteed by caller to be a valid interrupt vector index (< MAX_INTERRUPTS).
    let slot = unsafe { PDEV_INTERRUPTS.slot_unchecked(vector.0 as usize) };
    // Use a relaxed load as permanent handlers are never modified once set, and they are only set in
    // startup code, so there is nothing to race with.
    if slot.permanent.load(Ordering::Relaxed) {
        // Once permanent is set to true we know that handler is immutable and so it is safe
        // to read without holding the lock.
        slot.handler.invoke();
        return true;
    }

    ksync::lock!(let guard = PDEV_INTERRUPTS.lock_lock());
    let slot = &guard.fields().table[vector.0 as usize];
    if slot.handler.present() {
        slot.handler.invoke();
        true
    } else {
        false
    }
}

/// Registers an interrupt handler for the specified vector.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
unsafe fn register_int_handler_common(
    vector: InterruptVector,
    handler: InterruptHandler,
    permanent: bool,
) -> Result<(), Status> {
    if !get_driver().is_valid(vector, 0) {
        return Err(Status::INVALID_ARGS);
    }

    ksync::lock!(let mut guard = PDEV_INTERRUPTS.lock_lock());
    let slot = &mut guard.as_mut().fields_mut().table[vector.0 as usize];
    if (handler.present() && slot.handler.present()) || slot.permanent.load(Ordering::Relaxed) {
        return Err(Status::ALREADY_BOUND);
    }

    slot.handler = handler;
    slot.permanent.store(permanent, Ordering::Relaxed);

    Ok(())
}

/// Registers an interrupt handler for the specified vector.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_int_handler(
    vector: InterruptVector,
    handler: InterruptHandler,
) -> Result<(), Status> {
    unsafe { register_int_handler_common(vector, handler, false) }
}

/// Registers a permanent interrupt handler for the specified vector.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_permanent_int_handler(
    vector: InterruptVector,
    handler: InterruptHandler,
) -> Result<(), Status> {
    unsafe { register_int_handler_common(vector, handler, true) }
}

/// Checks if an interrupt is registered
///
/// # Safety
///
/// The vector must be within valid range, and HANDLER_TABLE must be initialized.
pub fn is_interrupt_registered(vector: u32) -> bool {
    if vector as usize >= MAX_INTERRUPTS {
        return false;
    }
    ksync::lock!(let guard = PDEV_INTERRUPTS.lock_lock());
    let slot = &guard.fields().table[vector as usize];
    slot.handler.present()
}

/// Queries the status of an interrupt vector.
pub fn query_interrupt_status(vector: u32) -> (Option<bool>, Option<bool>) {
    match get_driver().status(InterruptVector(vector)) {
        Ok((pending, enabled)) => (Some(pending), Some(enabled)),
        Err(_) => (None, None),
    }
}

/// Queries the configuration of an interrupt vector.
pub fn query_interrupt_config(
    vector: u32,
) -> (Option<InterruptTriggerMode>, Option<InterruptPolarity>) {
    match get_driver().config(InterruptVector(vector)) {
        Ok((tm, pol)) => (Some(tm), Some(pol)),
        Err(_) => (None, None),
    }
}

/// Masks the specified interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered, and vector must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mask_interrupt(vector: InterruptVector) -> Result<(), Status> {
    get_driver().mask(vector)
}

/// Unmasks the specified interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered, and vector must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmask_interrupt(vector: InterruptVector) -> Result<(), Status> {
    get_driver().unmask(vector)
}

/// Deactivates the specified interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered, and vector must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deactivate_interrupt(vector: InterruptVector) -> Result<(), Status> {
    get_driver().deactivate(vector)
}

/// Configures the specified interrupt vector trigger mode and polarity.
///
/// # Safety
///
/// The global interrupt controller must be registered, and vector must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn configure_interrupt(
    vector: InterruptVector,
    tm: InterruptTriggerMode,
    pol: InterruptPolarity,
) -> Result<(), Status> {
    get_driver().configure(vector, tm, pol)
}

/// Gets the specified interrupt vector configuration.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `tm` and `pol` must each be null or valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn get_interrupt_config(
    vector: InterruptVector,
    tm: *mut InterruptTriggerMode,
    pol: *mut InterruptPolarity,
) -> Result<(), Status> {
    match get_driver().config(vector) {
        Ok((out_tm, out_pol)) => {
            // SAFETY: `tm` and `pol` are either null or valid pointers supplied by the caller.
            unsafe {
                if !tm.is_null() {
                    *tm = out_tm;
                }
                if !pol.is_null() {
                    *pol = out_pol;
                }
            }
            Ok(())
        }
        Err(status) => Err(status),
    }
}

/// Sets the interrupt affinity mask for the specified vector.
///
/// # Safety
///
/// The global interrupt controller must be registered, and vector must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_interrupt_affinity(
    vector: InterruptVector,
    mask: u32,
) -> Result<(), Status> {
    get_driver().set_affinity(vector, mask)
}

/// Gets the base interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn interrupt_get_base_vector() -> InterruptVector {
    get_driver().base_vector()
}

/// Gets the maximum interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn interrupt_get_max_vector() -> InterruptVector {
    get_driver().max_vector()
}

/// Checks if the given interrupt vector is valid.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn is_valid_interrupt(vector: InterruptVector, flags: u32) -> bool {
    get_driver().is_valid(vector, flags)
}

/// Remaps the specified interrupt vector.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn remap_interrupt(vector: InterruptVector) -> InterruptVector {
    get_driver().remap(vector)
}

/// Sends an IPI to the target CPU.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn interrupt_send_ipi(target: cpu_mask_t, ipi: MpIpi) -> Result<(), Status> {
    get_driver().send_ipi(target, ipi)
}

/// Initializes interrupts for the current CPU early in boot.
fn interrupt_init_percpu_early(_level: init::LkInitLevel) {
    get_driver().init_percpu_early();
}

/// Initializes interrupts for the current CPU.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn interrupt_init_percpu() {
    get_driver().init_percpu();
}

/// Handles a platform IRQ.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `frame` must point to a valid interrupt frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn platform_irq(frame: *mut Iframe) {
    // SAFETY: `frame` points to a valid interrupt frame supplied by the caller.
    let frame = unsafe { &mut *frame };
    get_driver().handle_irq(frame);
}

/// Shuts down all interrupts.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shutdown_interrupts() {
    get_driver().shutdown();
}

/// Shuts down interrupts for the current CPU.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shutdown_interrupts_curr_cpu() {
    get_driver().shutdown_cpu();
}

/// Suspends interrupts for the current CPU.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn suspend_interrupts_curr_cpu() -> Result<(), Status> {
    get_driver().suspend_cpu()
}

/// Resumes interrupts for the current CPU.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn resume_interrupts_curr_cpu() -> Result<(), Status> {
    get_driver().resume_cpu()
}

/// Checks if MSI is supported.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_is_supported() -> bool {
    get_driver().msi_is_supported()
}

/// Checks if MSI supports masking.
///
/// # Safety
///
/// The global interrupt controller must be registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_supports_masking() -> bool {
    get_driver().msi_supports_masking()
}

/// Masks or unmasks the specified MSI interrupt.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `block` must point to a valid, initialized `MsiBlock`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_mask_unmask(block: *const MsiBlock, msi_id: u32, mask: bool) {
    debug_assert!(!block.is_null());
    // SAFETY: `block` points at a valid, initialized `MsiBlock` owned by the caller for the
    // duration of the call.
    let block = unsafe { &*block };
    get_driver().msi_mask_unmask(block, msi_id, mask);
}

/// Allocates a block of MSIs.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `out_block` must point to a valid, initialized `MsiBlock` that the caller owns for the
///   duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_alloc_block(
    requested_irqs: u32,
    can_target_64bit: bool,
    is_msix: bool,
    out_block: *mut MsiBlock,
) -> Result<(), Status> {
    if out_block.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `out_block` was checked to be non-null, and the caller guarantees that it points
    // at an initialized block that it owns for the duration of the call.
    let out_block = unsafe { &mut *out_block };
    get_driver().msi_alloc_block(requested_irqs, can_target_64bit, is_msix, out_block)
}

/// Frees the specified block of MSIs.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `block` must point to a valid, previously allocated `MsiBlock` that needs to be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_free_block(block: *mut MsiBlock) {
    debug_assert!(!block.is_null());
    // SAFETY: `block` points at a valid, previously allocated `MsiBlock` owned by the caller for
    // the duration of the call.
    let block = unsafe { &mut *block };
    get_driver().msi_free_block(block);
}

/// Registers an interrupt handler for the specified MSI.
///
/// # Safety
///
/// - The global interrupt controller must be registered.
/// - `block` must point to a valid, initialized `MsiBlock`.
/// - `handler` must point to a valid interrupt handler function or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn msi_register_handler(
    block: *mut MsiBlock,
    msi_id: u32,
    handler: InterruptHandler,
) {
    debug_assert!(!block.is_null());
    // SAFETY: `block` points at a valid, initialized `MsiBlock` owned by the caller for the
    // duration of the call.
    let block = unsafe { &*block };
    get_driver().msi_register_handler(block, msi_id, handler);
}

/// PDEV interrupt layer kernel tests.
#[cfg(ktest)]
#[unittest::suite(name = "interrupts")]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use unittest::{assert_err, assert_false};

    /// Test default interrupt controller fallback behavior.
    #[test]
    fn test_pdev_default_ops_fallback() {
        assert_err!(DEFAULT_DRIVER.mask(InterruptVector(0)), Status::NOT_SUPPORTED);
        assert_err!(DEFAULT_DRIVER.unmask(InterruptVector(0)), Status::NOT_SUPPORTED);
    }

    /// Test unregistered interrupt vector state query.
    #[test]
    fn test_pdev_unregistered_interrupt_state() {
        assert_false!(is_interrupt_registered(999));
    }
}

init::lk_init_hook_flags!(
    interrupt_init_percpu_early,
    interrupt_init_percpu_early,
    init::LK_INIT_LEVEL_PLATFORM_EARLY,
    init::LkInitFlags::SecondaryCpus
);
