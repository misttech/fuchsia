// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! # Armv7MmioTimer
//!
//! `Armv7MmioTimer` and its associated inner struct, `Timer`, provide
//! an interface to the memory mapped timer described in the v7 ARM ARM.
//!
//! `Armv7MmioTimer` represents a block of timer registers, while
//! `Timer` represents the specific PCT and VCT based timers within
//! that block, when available.
//!
//! ## Usage.
//!
//! ### Finding a timer.
//!
//! After initialization, which takes place just before `LK_INIT_LEVEL_PLATFORM`,
//! users may find a timer to use with the `Armv7MmioTimer::get` method.  There are
//! a maximum of `Armv7MmioTimer::MAX_TIMERS` timer blocks in the system.  When a
//! given block index is not detected or is otherwise unusable, the
//! `Armv7MmioTimer::get` method will return `None`.
//!
//! Each block may contain a PCT based timer, a VCT based timer, or both.  They
//! may be accessed via the `pct_timer()` and `vct_timer()` methods of a timer block
//! instance, and are usable if their `supported()` members return `true`.
//!
//! ### Configuring a handler.
//!
//! In order to receive notification that a timer has fired, users must register
//! an `IrqHandler` with the timer via the `Timer::set_handler`
//! method.  This is an inline callable which takes no
//! arguments and returns an `IrqHandlerResult`.
//!
//! Only one handler can be registered at once, and attempts to register a second
//! handler will return an error.  In order to change an existing handler for a
//! `Timer` instance, users must first call `reset_handler`.
//!
//! ### Using the timer.
//!
//! #### Programming.
//!
//! After configuring a handler, users may read the timer's current clock value
//! via the `ticks()` method.  Additionally, the `Timer`'s tick rate may be read
//! from its parent timer block's `ticks_to_nsec()` method.  An absolute deadline
//! may be programmed via the `set_timer` method, which takes a deadline expressed
//! in the timer's specific ticks timeline.  Alternatively, a relative timer may
//! be set using the `set_relative_timer` helper method, which takes a deadline
//! expressed in nanosecond units.
//!
//! #### Canceling.
//!
//! A programmed timer may be canceled at any time by calling the `cancel_timer`
//! method.  While canceling a running timer is a race, the `Timer` instance
//! guarantees that after `cancel_timer` has been called, any handlers will have
//! been successfully canceled and will not run, or they will have already run to
//! completion.  Calls to `cancel_timer` are idempotent.
//!
//! #### Handling a timer event.
//!
//! When a timer finally fires, a user's registered handler will be called.  This
//! is done at hard IRQ time and with interrupts disabled.  Blocking is not
//! allowed.  The timer always acts as a one-shot.  When the timer handler has
//! been called, the timer is effectively canceled.  Users may safely reset their
//! timer during their timer handler by calling either `set_timer` or
//! `set_relative_timer`. They are also free to cancel their timer using `cancel_timer`,
//! however since timers are effectively already canceled at the start of the
//! handler, this would only matter if they had already set the timer during the
//! handler and suddenly wanted to cancel it.
//!
//! Users *may not* change their registered handler during their handler
//! callback.  Any attempt to do so will produce an error.  That said, timer
//! handlers may choose to stay registered after the handler has run by returning
//! `IrqHandlerResult::RemainRegistered`, or they may have their handler
//! unregistered after completing by returning `IrqHandlerResult::Unregister`.  Care
//! must be taken, however, as the destruction of the inline function will take
//! place at IRQ time.  In order to safely unregister the handler at this time,
//! the inline function must not hold any references to anything which would
//! require blocking to destroy (such as a heap allocation).

mod debug;
mod registers;

use crate::arch_rs::arm64::periph_paddr_to_vaddr;
use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num, ints_disabled};
use crate::kernel::types::{BOOT_CPU_ID, INVALID_CPU, PAddr, cpu_mask_t, cpu_num_to_mask};
use crate::pdev_interrupt::{
    InterruptHandler, InterruptPolarity, InterruptTriggerMode, InterruptVector,
    configure_interrupt, mask_interrupt, register_int_handler, set_interrupt_affinity,
    unmask_interrupt,
};
use crate::root_resource_filter::root_resource_filter_add_deny_region;
use ::debug::dprintf;
use affine::Ratio;
use core::ffi::{c_char, c_int, c_void};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use libarch::yield_processor;
use registers::{
    COUNTER_SUBVIEW_SIZE, CntCtl, MMIO_FRAME_SIZE, RegisterMmio, TIMER_SUBVIEW_SIZE, cnt_base,
    cnt_ctl_base, cnt_pv_regs,
};
use zbi::{DcfgArmGenericTimerMmioDriver, KernelDriverIrqFlags};
use zx_status::Status;
use zx_types::{ZX_RSRC_KIND_MMIO, zx_duration_t};

const NS_PER_SEC: u32 = 1_000_000_000;

zr::static_assert_size_and_align!(DcfgArmGenericTimerMmioDriver, 272, 8);
zr::static_assert_size_and_align!(Ratio, 8, 4);

/// Type of timer within an ARMv7 MMIO timer frame.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Pct = 0,
    Vct = 1,
}

/// Return value from an [`IrqHandler`] callback indicating whether to remain registered.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqHandlerResult {
    RemainRegistered = 0,
    Unregister = 1,
}

/// Interrupt configuration for a timer.
#[derive(Clone, Copy, Debug)]
pub struct Irq {
    /// Interrupt vector number, or 0 when the ZBI config provides none.
    pub num: u32,
    /// `ZBI_KERNEL_DRIVER_IRQ_FLAGS_*` trigger mode and polarity bits.
    pub zbi_flags: u32,
}

/// Pointer-sized inline callback for handling timer interrupts.
pub struct IrqHandler {
    cookie: MaybeUninit<*mut c_void>,
    call: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>) -> i32>,
    destroy: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>)>,
}

// SAFETY: `IrqHandler` only accepts `Send` closures in `IrqHandler::new` or caller-verified
// cross-CPU-safe callbacks in `IrqHandler::from_raw`.
unsafe impl Send for IrqHandler {}

impl Default for IrqHandler {
    fn default() -> Self {
        Self::empty()
    }
}

impl Drop for IrqHandler {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy.take() {
            // SAFETY: `self.cookie` holds the payload associated with `destroy`.
            unsafe { destroy(self.cookie) };
        }
        self.call = None;
        self.cookie = MaybeUninit::new(core::ptr::null_mut());
    }
}

impl IrqHandler {
    /// Creates an empty `IrqHandler`.
    pub const fn empty() -> Self {
        Self { cookie: MaybeUninit::new(core::ptr::null_mut()), call: None, destroy: None }
    }

    /// Creates an `IrqHandler` from raw C callback components.
    ///
    /// # Safety
    ///
    /// `cookie`, `call`, and `destroy` must be safe to transfer across CPUs. If `call` is `Some`,
    /// `cookie` must be valid to pass to `call` in hard IRQ context with interrupts disabled. If
    /// `destroy` is `Some`, `cookie` must be valid to pass to `destroy` once when the handler is
    /// dropped.
    pub const unsafe fn from_raw(
        cookie: MaybeUninit<*mut c_void>,
        call: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>) -> i32>,
        destroy: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>)>,
    ) -> Self {
        Self { cookie, call, destroy }
    }

    /// Creates an `IrqHandler` from a pointer-sized, `Copy` Rust closure.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() -> IrqHandlerResult + Copy + Send + 'static,
    {
        const {
            assert!(core::mem::size_of::<F>() <= core::mem::size_of::<*mut c_void>());
            assert!(core::mem::align_of::<F>() <= core::mem::align_of::<*mut c_void>());
        }

        /// # Safety
        ///
        /// `cookie` must have been initialized by writing a valid `F` at its start.
        unsafe extern "C" fn call_shim<F: Fn() -> IrqHandlerResult>(
            cookie: MaybeUninit<*mut c_void>,
        ) -> i32 {
            // SAFETY: `cookie` was initialized by writing `F` at its start.
            let f_ref = unsafe { &*cookie.as_ptr().cast::<F>() };
            f_ref() as i32
        }

        // Store the closure in `MaybeUninit<*mut c_void>` so padding bytes inside `F` remain valid
        // when `cookie` is copied or passed across FFI.
        let mut cookie: MaybeUninit<*mut c_void> = MaybeUninit::zeroed();
        // SAFETY: `F` fits within `MaybeUninit<*mut c_void>` and has alignment
        // `<= align_of::<*mut c_void>()`.
        unsafe {
            core::ptr::write(cookie.as_mut_ptr().cast::<F>(), f);
        }

        Self { cookie, call: Some(call_shim::<F>), destroy: None }
    }

    /// Returns `true` if a callback is present.
    pub const fn is_some(&self) -> bool {
        self.call.is_some()
    }

    /// Returns the raw `(cookie, call)` pair if a callback is registered.
    const fn callable(
        &self,
    ) -> Option<(MaybeUninit<*mut c_void>, unsafe extern "C" fn(MaybeUninit<*mut c_void>) -> i32)>
    {
        match self.call {
            Some(call) => Some((self.cookie, call)),
            None => None,
        }
    }
}

/// Pointer from a [`Timer`] back to the [`Armv7MmioTimer`] frame that contains it.
///
/// Safety invariant: points to the `Armv7MmioTimer` whose `pct_timer` or `vct_timer` field is this
/// `Timer`. That frame is allocated once in `Armv7MmioTimer::init`, leaked, never moved, and never
/// mutated again after it is published in `TIMERS`, so the pointee stays valid and shared-readable
/// for the lifetime of the kernel.
struct OwnerPtr(NonNull<Armv7MmioTimer>);

// SAFETY: `OwnerPtr` only grants shared access to an immutable, leaked `Armv7MmioTimer` (see the
// invariant above), so sharing or sending it between CPUs cannot create a data race.
unsafe impl Send for OwnerPtr {}
// SAFETY: See `Send` above.
unsafe impl Sync for OwnerPtr {}

/// Specific PCT or VCT timer within an [`Armv7MmioTimer`] frame.
#[ksync::guarded]
pub struct Timer {
    owner: OwnerPtr,
    counter_mmio: RegisterMmio,
    timer_mmio: RegisterMmio,
    timer_type: Type,
    irq: Irq,
    supported: bool,

    #[mutex]
    irq_lock: ksync::KSpinlock,

    #[guarded_by(irq_lock)]
    user_handler: IrqHandler,

    #[guarded_by(irq_lock)]
    enabled: bool,

    active_cpu: AtomicU32,
}

impl Timer {
    fn init(
        owner: NonNull<Armv7MmioTimer>,
        timer_type: Type,
        irq: Irq,
    ) -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            owner: OwnerPtr(owner),
            counter_mmio: RegisterMmio::empty(),
            timer_mmio: RegisterMmio::empty(),
            timer_type,
            irq,
            supported: false,
            irq_lock <- ksync::KSpinlock::init(),
            user_handler: ksync::KCell::new(IrqHandler::empty()),
            enabled: ksync::KCell::new(false),
            active_cpu: AtomicU32::new(INVALID_CPU),
        })
    }

    /// Returns the [`Armv7MmioTimer`] frame that contains this timer.
    fn owner(&self) -> &Armv7MmioTimer {
        // SAFETY: By `OwnerPtr`'s invariant the pointee is a leaked, immutable `Armv7MmioTimer`
        // that outlives every `Timer`, so a shared reference to it is valid for any lifetime.
        unsafe { self.owner.0.as_ref() }
    }

    fn set_supported(&mut self, value: bool) {
        self.supported = value;
    }

    /// Returns `true` if the hardware and ZBI config make this timer usable.
    pub const fn supported(&self) -> bool {
        self.supported
    }

    /// Returns `true` if a deadline is currently programmed.
    pub fn enabled(&self) -> bool {
        ksync::lock!(let guard = self.lock_irq_lock());
        *guard.fields().enabled
    }

    /// Returns `true` if an [`IrqHandler`] is currently registered.
    pub fn has_handler(&self) -> bool {
        ksync::lock!(let guard = self.lock_irq_lock());
        guard.fields().user_handler.is_some()
    }

    /// Returns the interrupt vector number, or 0 if none was configured.
    pub const fn irq(&self) -> u32 {
        self.irq.num
    }

    /// Returns whether this is the PCT or VCT timer of its frame.
    pub const fn timer_type(&self) -> Type {
        self.timer_type
    }

    /// Returns `"PCT"` or `"VCT"`.
    pub const fn type_name(&self) -> &'static str {
        match self.timer_type {
            Type::Pct => "PCT",
            Type::Vct => "VCT",
        }
    }

    /// Returns `"PCT"` or `"VCT"` as a static NUL-terminated string for C++ callers.
    const fn type_name_cstr(&self) -> *const c_char {
        match self.timer_type {
            Type::Pct => c"PCT".as_ptr(),
            Type::Vct => c"VCT".as_ptr(),
        }
    }

    /// Reads the current value of this timer's counter.
    pub fn ticks(&self) -> u64 {
        self.counter_mmio.read64(cnt_pv_regs::CT)
    }

    /// Maps the counter and timer register sub-views, disables the timer, and if it is supported
    /// configures and unmasks its interrupt with `this` as the handler cookie.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, uniquely owned `Self` within a leaked `Armv7MmioTimer` that
    /// never moves or is freed after this call: its address is registered with the interrupt
    /// subsystem for the lifetime of the kernel.
    unsafe fn setup(this: NonNull<Self>, owner_mmio: RegisterMmio, owner_frame_ndx: u32) {
        let ptr = this.as_ptr();
        // SAFETY: `this` is uniquely owned prior to publication in `TIMERS`. Operating through
        // `ptr` avoids forming a temporary `&mut Self` retag so the registered interrupt cookie
        // retains root allocation provenance.
        let (supported, irq, zbi_flags, type_name, timer_mmio) = unsafe {
            let timer_type = (*ptr).timer_type;
            let counter_offset =
                if timer_type == Type::Pct { cnt_base::CNTPCT_ADDR } else { cnt_base::CNTVCT_ADDR };
            let timer_offset = if timer_type == Type::Pct {
                cnt_base::CNTP_CVAL_ADDR
            } else {
                cnt_base::CNTV_CVAL_ADDR
            };

            let counter_mmio = owner_mmio.subview(counter_offset, COUNTER_SUBVIEW_SIZE);
            let timer_mmio = owner_mmio.subview(timer_offset, TIMER_SUBVIEW_SIZE);
            (*ptr).counter_mmio = counter_mmio;
            (*ptr).timer_mmio = timer_mmio;

            ((*ptr).supported, (*ptr).irq(), (*ptr).irq.zbi_flags, (*ptr).type_name(), timer_mmio)
        };

        // unconditionally disable the timer and mask it at the timer register level.
        timer_mmio.write32(cnt_pv_regs::CTL, CntCtl::from(0).set_enable(false).set_imask(true));

        if supported {
            debug_assert!(irq != 0);
            // SAFETY: The interrupt controller ops are registered before platform device init
            // runs; the vector is whatever the ZBI config supplied, and the controller rejects
            // out-of-range vectors with an error rather than faulting.
            let _ = unsafe { mask_interrupt(InterruptVector(irq)) };

            let mode_flags = zbi_flags
                & (KernelDriverIrqFlags::EDGE_TRIGGERED.bits()
                    | KernelDriverIrqFlags::LEVEL_TRIGGERED.bits());
            let polarity_flags = zbi_flags
                & (KernelDriverIrqFlags::POLARITY_LOW.bits()
                    | KernelDriverIrqFlags::POLARITY_HIGH.bits());
            if (mode_flags.count_ones() == 1) || (polarity_flags.count_ones() == 1) {
                let irq_mode = if (mode_flags & KernelDriverIrqFlags::EDGE_TRIGGERED.bits()) != 0 {
                    InterruptTriggerMode::Edge
                } else {
                    InterruptTriggerMode::Level
                };
                let irq_polarity =
                    if (polarity_flags & KernelDriverIrqFlags::POLARITY_LOW.bits()) != 0 {
                        InterruptPolarity::Low
                    } else {
                        InterruptPolarity::High
                    };

                // SAFETY: The interrupt controller ops are registered and reject out-of-range
                // vectors (see `mask_interrupt` above). The cookie `ptr` carries the root
                // allocation provenance of the leaked `Armv7MmioTimer` frame, which never moves
                // or is freed. The hardware timer was disabled and masked via `CTL` above, so no
                // interrupt can observe `this` before setup finishes.
                unsafe {
                    let _ = configure_interrupt(InterruptVector(irq), irq_mode, irq_polarity);
                    let _ = register_int_handler(
                        InterruptVector(irq),
                        InterruptHandler {
                            cookie: ptr.cast(),
                            r#fn: Some(Self::irq_handler_thunk_helper),
                        },
                    );
                    let _ = unmask_interrupt(InterruptVector(irq));
                }
            } else {
                // SAFETY: `this` is uniquely owned prior to publication in `TIMERS`.
                unsafe {
                    (*ptr).supported = false;
                }
                dprintf!(
                    INFO,
                    "Bad IRQ flags for {} timer frame {} (flags 0x{:08x})\n",
                    type_name,
                    owner_frame_ndx,
                    zbi_flags
                );
            }
        }
    }

    /// Returns the time remaining until the programmed deadline, or 0 if it has already passed.
    ///
    /// Fails with `NOT_SUPPORTED` if the timer is unusable and `BAD_STATE` if no deadline is set.
    pub fn time_until_deadline(&self) -> Result<zx_duration_t, Status> {
        if !self.supported {
            return Err(Status::NOT_SUPPORTED);
        }

        ksync::lock!(let guard = self.lock_irq_lock());
        if !*guard.fields().enabled {
            return Err(Status::BAD_STATE);
        }

        let deadline = self.timer_mmio.read64(cnt_pv_regs::CVAL);
        let now = self.ticks();
        if deadline <= now {
            return Ok(0);
        }

        Ok(self.owner().ticks_to_nsec.scale::<{ affine::Round::DOWN }>((deadline - now) as i64))
    }

    extern "C" fn irq_handler_thunk_helper(cookie: *mut c_void) {
        // SAFETY: `cookie` was registered in `Timer::setup` as a pointer to this `Timer`, which
        // lives in a leaked `Armv7MmioTimer` frame. No `&mut Timer` exists once `setup` returns,
        // so a shared reference is valid for the duration of the interrupt.
        let timer = unsafe { &*cookie.cast::<Self>() };
        timer.irq_handler_thunk();
    }

    fn irq_handler_thunk(&self) {
        debug_assert!(ints_disabled());
        ksync::lock!(let mut guard = self.lock_irq_lock_policy::<ksync::NoIrqSavePolicy>());

        // If someone canceled us after the handler fired, but before we made it to
        // this point, then the Cancel operation won the race.  Don't run the handler,
        // just get out.
        if !*guard.fields().enabled {
            return;
        }

        // Is our deadline in the future?  This is possible if someone reset the
        // deadline on another CPU after the interrupt fired, but before it made it
        // to here.  If this happens, the handler lost the race, and we should just
        // get out.
        let deadline = self.timer_mmio.read64(cnt_pv_regs::CVAL);
        let now = self.ticks();
        if deadline >= now {
            return;
        }

        // Looks like the timer fired while we were still enabled.  Disable the timer,
        // and dispatch the handler if we have one registered.
        self.disable_locked(guard.as_mut().fields_mut().enabled);

        if let Some((cookie, call)) = guard.fields().user_handler.callable() {
            // We have a handler.  Record that fact that we have an active IRQ handler
            // in flight by recording our CPU number in the `active_cpu` member
            // variable, then run the handler.
            let current_cpu = curr_cpu_num();
            self.active_cpu.store(current_cpu, Ordering::Relaxed);

            // Copy the callback out of `user_handler` and run it with the lock dropped.  The
            // copy stays valid because `set_handler` refuses to replace `user_handler` while
            // `active_cpu` is something other than INVALID_CPU.
            let irq_result = guard.as_mut().call_unlocked(|| {
                // SAFETY: The payload behind `cookie` is owned by `user_handler`, which cannot
                // be replaced or destroyed while `active_cpu != INVALID_CPU`.
                unsafe { call(cookie) }
            });

            // Now that we are back inside of the lock, but before we mark ourselves as
            // no longer in flight, If the interrupt handler asked us to unregister, do
            // so now.  The handler is destroyed here, under the lock and at IRQ time.
            if irq_result == IrqHandlerResult::Unregister as i32 {
                drop(core::mem::take(guard.as_mut().fields_mut().user_handler));
            }

            // We are back inside of the `irq_lock` once again.  The IRQ handler is
            // finished, reset the `active_cpu` back to INVALID_CPU.
            self.active_cpu.store(INVALID_CPU, Ordering::Release);
        }
    }

    /// Installs `handler` as the interrupt callback and pins the interrupt to `mask`.
    ///
    /// Fails with `BAD_STATE` if a handler is already registered (call [`Timer::reset_handler`]
    /// first) or if called from within the handler itself. Any previous handler is destroyed
    /// after the lock is released.
    pub fn set_handler(&self, handler: IrqHandler, mask: cpu_mask_t) -> Result<(), Status> {
        let _irqd = InterruptDisableGuard::new();

        // If we are resetting our handler, make sure to destroy the old handler
        // outside of the spinlock code by transferring it to old_handler first before
        // dropping the lock.
        let mut _old_handler = IrqHandler::empty();
        let current_cpu = curr_cpu_num();
        loop {
            ksync::lock!(let mut guard = self.lock_irq_lock_policy::<ksync::NoIrqSavePolicy>());

            // Users are not allowed to replace and existing handler with a new existing handler.
            // They must explicitly reset the existing handler first.
            if guard.fields().user_handler.is_some() && handler.is_some() {
                return Err(Status::BAD_STATE);
            }

            // Is there an IRQ handler in flight?  If it is running on a different CPU,
            // drop our lock and wait for it to finish.  If it is running on the same
            // CPU as us, then it is in the process of calling us.  It is not legal to
            // reset the IRQ handler from within the IRQ handler itself.
            let active_cpu = self.active_cpu.load(Ordering::Acquire);
            if active_cpu != INVALID_CPU {
                if active_cpu != current_cpu {
                    guard.release();
                    yield_processor();
                    continue;
                }
                return Err(Status::BAD_STATE);
            }

            self.disable_locked(guard.as_mut().fields_mut().enabled);

            // SAFETY: The interrupt controller ops are registered and reject out-of-range
            // vectors; we hold `irq_lock` with the timer disabled, so no interrupt is in flight.
            unsafe { set_interrupt_affinity(InterruptVector(self.irq()), mask) }?;

            _old_handler = core::mem::replace(guard.as_mut().fields_mut().user_handler, handler);

            return Ok(());
        }
    }

    /// Removes any registered handler and returns the interrupt affinity to the boot CPU.
    pub fn reset_handler(&self) -> Result<(), Status> {
        self.set_handler(IrqHandler::empty(), cpu_num_to_mask(BOOT_CPU_ID))
    }

    /// Disables the timer and waits for any in-flight handler on another CPU to finish.
    ///
    /// On return the handler has either completed or will not run. Idempotent; may be called
    /// from within the handler itself.
    pub fn cancel_timer(&self) -> Result<(), Status> {
        if !self.supported {
            return Err(Status::NOT_SUPPORTED);
        }

        // Make sure that interrupts remain disabled, even after we drop the lock to
        // sync with any in-flight IRQ handler.
        let _irqd = InterruptDisableGuard::new();
        {
            // Lock and make sure the timer is disabled.
            ksync::lock!(let mut guard = self.lock_irq_lock());
            self.disable_locked(guard.as_mut().fields_mut().enabled);
        }

        // Now that we have dropped the lock, sync with any IrqHandler which may
        // currently be in flight. There is a handler in flight if the `active_cpu`
        // member is not INVALID_CPU.  Interrupts are currently disabled, so if the
        // `active_cpu` member is valid, and equal to our current cpu, then this is a
        // call to Cancel being made from the IrqHandler itself, and we can
        // immediately return.  Otherwise we wait until we see the active CPU achieve
        // INVALID at least once.
        let current_cpu = curr_cpu_num();
        loop {
            let active_cpu = self.active_cpu.load(Ordering::Acquire);
            if (active_cpu == current_cpu) || (active_cpu == INVALID_CPU) {
                break;
            }
            yield_processor();
        }

        Ok(())
    }

    /// Programs an absolute deadline in this timer's ticks and enables it.
    ///
    /// Waits for any in-flight handler on another CPU to finish first.
    pub fn set_timer(&self, ticks_deadline: u64) -> Result<(), Status> {
        if !self.supported {
            return Err(Status::NOT_SUPPORTED);
        }

        let _irqd = InterruptDisableGuard::new();
        let current_cpu = curr_cpu_num();

        loop {
            ksync::lock!(let mut guard = self.lock_irq_lock_policy::<ksync::NoIrqSavePolicy>());
            // If there is an IRQ handler in flight, and it is not running on this CPU
            // (meaning it is calling us), wait until it completes before setting the
            // timer.
            let active_cpu = self.active_cpu.load(Ordering::Acquire);
            if (active_cpu != current_cpu) && (active_cpu != INVALID_CPU) {
                guard.release();
                yield_processor();
                continue;
            }

            self.disable_locked(guard.as_mut().fields_mut().enabled);
            self.timer_mmio.write64(cnt_pv_regs::CVAL, ticks_deadline);
            self.enable_locked(guard.as_mut().fields_mut().enabled);
            break;
        }

        Ok(())
    }

    /// Programs a deadline `relative_timeout` nanoseconds from now; see [`Timer::set_timer`].
    pub fn set_relative_timer(&self, relative_timeout: zx_duration_t) -> Result<(), Status> {
        let nsec_to_ticks = self.owner().ticks_to_nsec.inverse();
        let delta_ticks = nsec_to_ticks.scale::<{ affine::Round::DOWN }>(relative_timeout);
        self.set_timer(self.ticks().wrapping_add(delta_ticks as u64))
    }

    fn disable_locked(&self, enabled: &mut bool) {
        self.timer_mmio
            .write32(cnt_pv_regs::CTL, CntCtl::from(0).set_enable(false).set_imask(true));
        *enabled = false;
    }

    fn enable_locked(&self, enabled: &mut bool) {
        self.timer_mmio
            .write32(cnt_pv_regs::CTL, CntCtl::from(0).set_enable(true).set_imask(false));
        *enabled = true;
    }
}

static MMIO_CTL_BASE: AtomicUsize = AtomicUsize::new(0);
static TIMERS: [AtomicPtr<Armv7MmioTimer>; Armv7MmioTimer::MAX_TIMERS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; Armv7MmioTimer::MAX_TIMERS];

/// Block of ARMv7 MMIO generic timer registers corresponding to a single timer frame.
#[pin_init::pin_data]
pub struct Armv7MmioTimer {
    frame_ndx: u32,
    mmio: RegisterMmio,
    el0_mmio: RegisterMmio,
    ticks_to_nsec: Ratio,
    #[pin]
    pct_timer: Timer,
    #[pin]
    vct_timer: Timer,
}

impl Armv7MmioTimer {
    /// Maximum number of timer frames the system can expose.
    pub const MAX_TIMERS: usize = 8;

    /// Compile-time check that the ZBI config carries exactly `MAX_TIMERS` frames.
    fn frames<T, const N: usize>(frames: &[T; N]) -> &[T; N] {
        const {
            assert!(N == Self::MAX_TIMERS);
        }
        frames
    }

    fn mmio_ctl() -> RegisterMmio {
        let base = MMIO_CTL_BASE.load(Ordering::Acquire);
        if base == 0 {
            RegisterMmio::empty()
        } else {
            // SAFETY: `MMIO_CTL_BASE` is only set in `init` after translating the physical base
            // address via `periph_paddr_to_vaddr` for a 4 KiB MMIO page.
            unsafe { RegisterMmio::new(base, MMIO_FRAME_SIZE) }
        }
    }

    /// Returns timer frame `ndx`, or `None` if it is absent, disabled, or failed to initialize.
    pub fn get(ndx: usize) -> Option<&'static Self> {
        if ndx < Self::MAX_TIMERS {
            let ptr = TIMERS[ndx].load(Ordering::Acquire);
            // SAFETY: Non-null pointers in `TIMERS` were allocated via `kalloc::Box`, fully
            // initialized before the `Release` store that published them, and leaked. No `&mut
            // Self` is ever formed after publication, so shared `'static` access is sound.
            unsafe { ptr.as_ref() }
        } else {
            None
        }
    }

    fn pin_init(
        frame_ndx: u32,
        mmio: RegisterMmio,
        el0_mmio: RegisterMmio,
        phys_irq: Irq,
        virt_irq: Irq,
    ) -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(&this in Self {
            frame_ndx,
            mmio,
            el0_mmio,
            ticks_to_nsec: Ratio::default(),
            pct_timer <- Timer::init(this, Type::Pct, phys_irq),
            vct_timer <- Timer::init(this, Type::Vct, virt_irq),
        })
    }

    /// Discovers and initializes every timer frame described by `config`.
    ///
    /// Runs once during platform device init, before `LK_INIT_LEVEL_PLATFORM`. Frames that are
    /// absent, disabled by `active_frames_mask`, or fail to map are skipped with a log line.
    pub fn init(config: &DcfgArmGenericTimerMmioDriver) {
        // Add the top level control page and all of the EL1 timer register pages to
        // the root resource filter deny list.  We don't want user-mode to be able to
        // access these pages (although, it is OK if they can access the EL0 view).
        if config.mmio_phys != 0 {
            root_resource_filter_add_deny_region(
                config.mmio_phys as usize,
                MMIO_FRAME_SIZE,
                ZX_RSRC_KIND_MMIO,
            );
        }

        let frames = Self::frames(&config.frames);
        for frame in frames {
            if frame.mmio_phys_el1 != 0 {
                root_resource_filter_add_deny_region(
                    frame.mmio_phys_el1 as usize,
                    MMIO_FRAME_SIZE,
                    ZX_RSRC_KIND_MMIO,
                );
            }
            if frame.mmio_phys_el0 != 0 {
                root_resource_filter_add_deny_region(
                    frame.mmio_phys_el0 as usize,
                    MMIO_FRAME_SIZE,
                    ZX_RSRC_KIND_MMIO,
                );
            }
        }

        let mmio_ctl = if let Some(virt_base) =
            periph_paddr_to_vaddr(PAddr(config.mmio_phys as usize))
            && virt_base.0 != 0
        {
            MMIO_CTL_BASE.store(virt_base.0, Ordering::Release);
            // SAFETY: `virt_base` is a valid peripheral mapping for the 4 KiB control frame.
            unsafe { RegisterMmio::new(virt_base.0, MMIO_FRAME_SIZE) }
        } else {
            dprintf!(
                CRITICAL,
                "Failed to translate ARMv7 Timer phys base @ 0x{:08x}\n",
                config.mmio_phys
            );
            return;
        };

        let tidr = mmio_ctl.read32(cnt_ctl_base::CNTTIDR);
        for frame_number in 0..Self::MAX_TIMERS as u32 {
            // Don't attempt to initialize a timer if the ID register says it does not
            // exist.
            if !tidr.implemented(frame_number) {
                continue;
            }

            if !TIMERS[frame_number as usize].load(Ordering::Acquire).is_null() {
                dprintf!(INFO, "ARMv7 Timer Frame {} has already been initialized\n", frame_number);
                continue;
            }

            let frame = &frames[frame_number as usize];
            if frame.mmio_phys_el1 == 0 {
                continue;
            }

            let el1_mmio =
                periph_paddr_to_vaddr(PAddr(frame.mmio_phys_el1 as usize)).map_or(0, |v| v.0);
            if el1_mmio == 0 {
                dprintf!(
                    INFO,
                    "Failed to translate EL1 timer base @ 0x{:08x} for ARMv7 timer frame {}\n",
                    frame.mmio_phys_el1,
                    frame_number
                );
                continue;
            }
            // SAFETY: `el1_mmio` is a valid peripheral mapping for the 4 KiB EL1 frame.
            let mmio = unsafe { RegisterMmio::new(el1_mmio, MMIO_FRAME_SIZE) };

            let el0_mmio = if frame.mmio_phys_el0 != 0 {
                let el0_vaddr =
                    periph_paddr_to_vaddr(PAddr(frame.mmio_phys_el0 as usize)).map_or(0, |v| v.0);
                if el0_vaddr == 0 {
                    dprintf!(
                        INFO,
                        "Failed to translate EL0 timer base @ 0x{:08x} for ARMv7 timer frame {}\n",
                        frame.mmio_phys_el0,
                        frame_number
                    );
                    RegisterMmio::empty()
                } else {
                    // Now that we have what appears to be a valid frame addresses for both
                    // the EL1 and EL0 frames, unconditionally deny timer access to EL0.  We
                    // don't want to accidentally give user mode access to a high resolution
                    // timer if they are not suppose to have it, regardless of whether or
                    // not we are going to skip the timer based on device tree
                    // configuration, we still want to make sure that EL0 does not have
                    // access.
                    //
                    // Note, it still should be pretty difficult for EL0 to access the timer
                    // registers since we added them to the root resource filter deny list
                    // at the start of Init, but it never hurts to add some extra
                    // roadblocks.
                    mmio.write32(cnt_base::CNTEL0ACR, cnt_base::CntEl0Acr::from(0));
                    // SAFETY: `el0_vaddr` is a valid peripheral mapping for the 4 KiB EL0 frame.
                    unsafe { RegisterMmio::new(el0_vaddr, MMIO_FRAME_SIZE) }
                }
            } else {
                RegisterMmio::empty()
            };

            // Finally, don't attempt to expose a timer which is not in the device-tree
            // "active frames" mask.
            //
            // Note: sometimes it is the case that there exist timers as reported by the
            // TIDR register which are not part of the active mask supplied by device
            // tree, even though the device tree _does_ provide both valid frame
            // addresses and interrupts.
            //
            // It is not immediately clear why device implementers might choose to do
            // this.  In theory, they might be trying to reserve these timers for use by
            // EL2/EL3, or perhaps for secure mode execution.  The main problem with
            // this theory is that those exception levels already have access to tools
            // that they can use to lock out EL0/EL1 access to the timers, in the form
            // of the CNTACR<n> and CNTNSAR registers.  If things like the secure
            // monitor wanted to reserve these timers for their own usage, why don't
            // they just deny access to EL0/EL1?
            //
            // Regardless, we skip them for now.
            //
            if (config.active_frames_mask & (1u8 << frame_number)) == 0 {
                dprintf!(
                    INFO,
                    "Explicitly skipping ARMv7 Timer Frame {}.  DeviceTree declares it as \
                     disabled.\n",
                    frame_number
                );
                continue;
            }

            let Ok(uninit_timer) = kalloc::Box::<Self>::try_new_uninit() else {
                dprintf!(INFO, "Failed to allocated ARMv7 Timer for Frame {}\n", frame_number);
                continue;
            };
            let timer_ptr: *mut Self = kalloc::Box::into_raw(uninit_timer).cast::<Self>();
            let init = Self::pin_init(
                frame_number,
                mmio,
                el0_mmio,
                Irq { num: frame.irq_phys, zbi_flags: frame.irq_phys_flags },
                Irq { num: frame.irq_virt, zbi_flags: frame.irq_virt_flags },
            );
            // SAFETY: `timer_ptr` points to a valid uninitialized heap allocation for `Self` that
            // remains pinned and leaked for the lifetime of the kernel, and `Self::setup`
            // completes all field writes before publishing `timer_ptr` to `TIMERS`.
            unsafe {
                let Ok(()) = pin_init::PinInit::__pinned_init(init, timer_ptr);
                Self::setup(timer_ptr, &mmio_ctl);
            }
            TIMERS[frame_number as usize].store(timer_ptr, Ordering::Release);
        }
    }

    /// Completes hardware inspection and interrupt setup for `this` after `PinInit::__pinned_init`
    /// without forming an exclusive `&mut Self` over the parent struct that would invalidate
    /// `owner: NonNull<Self>` provenance in `pct_timer` and `vct_timer`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid, uniquely owned `Self` allocation initialized by
    /// `PinInit::__pinned_init` prior to publication in `TIMERS`.
    unsafe fn setup(this: *mut Self, mmio_ctl: &RegisterMmio) {
        // SAFETY: `this` is uniquely owned prior to publication in `TIMERS`. Updating
        // `ticks_to_nsec` and the `supported` flags through `this` before calling `Timer::setup`
        // preserves the root allocation provenance shared with `owner: NonNull<Self>`.
        let (mmio, frame_ndx, pct_ptr, vct_ptr) = unsafe {
            let frame_ndx = (*this).frame_ndx;
            let mmio = (*this).mmio;
            let tidr = mmio_ctl.read32(cnt_ctl_base::CNTTIDR);
            let acr = mmio_ctl.read32(cnt_ctl_base::cnt_acr(frame_ndx));

            let freq = if acr.rfrq() { mmio.read32(cnt_base::CNTFRQ) } else { 0 };
            if freq != 0 {
                let mut ratio = Ratio::new(NS_PER_SEC, freq);
                ratio.reduce();
                (*this).ticks_to_nsec = ratio;
                let pct_irq = (*this).pct_timer.irq();
                (*this).pct_timer.set_supported(acr.rpct() && acr.rwpt() && (pct_irq != 0));
                let vct_irq = (*this).vct_timer.irq();
                (*this).vct_timer.set_supported(tidr.virt_impl(frame_ndx) && (vct_irq != 0));
            }
            (
                mmio,
                frame_ndx,
                NonNull::new_unchecked(&raw mut (*this).pct_timer),
                NonNull::new_unchecked(&raw mut (*this).vct_timer),
            )
        };

        // SAFETY: `pct_ptr` and `vct_ptr` point to the disjoint `pct_timer` and `vct_timer` fields
        // within `this`. `this` is a leaked heap allocation that is never moved or freed, which is
        // what `Timer::setup` requires of the address it registers, and it is not mutated again
        // after the IRQ handlers are registered.
        unsafe {
            Timer::setup(pct_ptr, mmio, frame_ndx);
            Timer::setup(vct_ptr, mmio, frame_ndx);
        }
    }

    /// Returns the ratio converting this frame's counter ticks to nanoseconds.
    pub const fn ticks_to_nsec(&self) -> Ratio {
        self.ticks_to_nsec
    }

    /// Returns the physical-counter timer of this frame.
    pub const fn pct_timer(&self) -> &Timer {
        &self.pct_timer
    }

    /// Returns the virtual-counter timer of this frame.
    pub const fn vct_timer(&self) -> &Timer {
        &self.vct_timer
    }
}

// ============================================================================
// C++ FFI Entrypoints
// ============================================================================

/// Initializes all ARMv7 MMIO timers described by `config`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_init(config: &DcfgArmGenericTimerMmioDriver) {
    Armv7MmioTimer::init(config);
}

/// Returns a pointer to the `Armv7MmioTimer` for frame `ndx`, or null if unavailable.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_get(ndx: usize) -> *mut Armv7MmioTimer {
    match Armv7MmioTimer::get(ndx) {
        Some(timer) => core::ptr::from_ref(timer).cast_mut(),
        None => core::ptr::null_mut(),
    }
}

/// Dumps register state for all timers matching `timer_mask`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_dump(timer_mask: u8) -> c_int {
    Armv7MmioTimer::dump(timer_mask)
}

/// Shows status for all timers matching `timer_mask`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_show_status(timer_mask: u8) -> c_int {
    Armv7MmioTimer::show_status(timer_mask)
}

/// Returns a reference to the `ticks_to_nsec` ratio of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_ticks_to_nsec(timer: &Armv7MmioTimer) -> &Ratio {
    &timer.ticks_to_nsec
}

/// Returns a reference to the physical counter timer (`pct_timer`) of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_pct_timer(timer: &Armv7MmioTimer) -> &Timer {
    &timer.pct_timer
}

/// Returns a reference to the virtual counter timer (`vct_timer`) of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_vct_timer(timer: &Armv7MmioTimer) -> &Timer {
    &timer.vct_timer
}

/// Returns whether `timer` is supported.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_supported(timer: &Timer) -> bool {
    timer.supported()
}

/// Returns whether `timer` is currently enabled.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_enabled(timer: &Timer) -> bool {
    timer.enabled()
}

/// Returns whether `timer` currently has an IRQ handler registered.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_has_handler(timer: &Timer) -> bool {
    timer.has_handler()
}

/// Returns the IRQ number of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_irq(timer: &Timer) -> u32 {
    timer.irq()
}

/// Returns the type of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_type(timer: &Timer) -> Type {
    timer.timer_type()
}

/// Returns the null-terminated name (`"PCT"` or `"VCT"`) of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_type_name(timer: &Timer) -> *const c_char {
    timer.type_name_cstr()
}

/// Reads and returns the current counter value of `timer` in ticks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_ticks(timer: &Timer) -> u64 {
    timer.ticks()
}

/// Sets or clears the interrupt handler for `timer`.
///
/// # Safety
///
/// `cookie`, `call`, and `destroy` must satisfy the safety requirements of
/// [`IrqHandler::from_raw`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_armv7_mmio_timer_timer_set_handler(
    timer: &Timer,
    cookie: MaybeUninit<*mut c_void>,
    call: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>) -> i32>,
    destroy: Option<unsafe extern "C" fn(MaybeUninit<*mut c_void>)>,
    mask: cpu_mask_t,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees handler parameters satisfy `IrqHandler::from_raw`.
    let handler = unsafe { IrqHandler::from_raw(cookie, call, destroy) };
    timer.set_handler(handler, mask)
}

/// Cancels any pending deadline on `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_cancel_timer(timer: &Timer) -> Result<(), Status> {
    timer.cancel_timer()
}

/// Programs an absolute deadline in ticks on `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_set_timer(
    timer: &Timer,
    ticks_deadline: u64,
) -> Result<(), Status> {
    timer.set_timer(ticks_deadline)
}

/// Programs a relative timeout in nanoseconds on `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_set_relative_timer(
    timer: &Timer,
    relative_timeout: zx_duration_t,
) -> Result<(), Status> {
    timer.set_relative_timer(relative_timeout)
}

/// Computes the remaining duration until the programmed deadline of `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_timer_timer_time_until_deadline(
    timer: &Timer,
    out_duration: &mut zx_duration_t,
) -> Result<(), Status> {
    *out_duration = timer.time_until_deadline()?;
    Ok(())
}
