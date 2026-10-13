// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use affine::Round;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, RawCriticalMutex, RawSpinlock, guarded, lock};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_INTERRUPT_RIGHTS, ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_INTERRUPT, zx_info_interrupt_t,
    zx_instant_boot_t, zx_koid_t, zx_rights_t, zx_time_t,
};

use super::interrupt_event_dispatcher::InterruptEventDispatcher;
use super::msi_interrupt_dispatcher::MsiInterruptDispatcher;
use super::port_dispatcher::{PortDispatcher, PortInterruptPacket};
use super::thread_dispatcher::{AutoBlocked, Blocked};
use super::virtual_interrupt_dispatcher::VirtualInterruptDispatcher;
use crate::kernel::deadline::Deadline;
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::AutoPreemptDisabler;
use crate::platform_rs::timer::{
    CurrentTicksObservation, current_boot_time, timer_current_mono_and_boot_ticks,
    timer_get_ticks_to_time_ratio,
};

unsafe extern "C" {
    fn cpp_interrupt_dispatcher_create(
        flags: u32,
        options: u32,
        ctx: *mut u8,
        init_fn: unsafe extern "C" fn(disp: &InterruptDispatcher, ctx: *mut u8),
        out: *mut *mut InterruptDispatcher,
    ) -> zx_types::zx_status_t;
    fn cpp_interrupt_dispatcher_wake_event_initialize(disp: &InterruptDispatcher);
    fn cpp_interrupt_dispatcher_wake_event_destroy(disp: &InterruptDispatcher);
    fn cpp_interrupt_dispatcher_wake_event_trigger(
        disp: &InterruptDispatcher,
        timestamp: zx_time_t,
    );
    fn cpp_interrupt_dispatcher_wake_event_acknowledge(
        disp: &InterruptDispatcher,
        remain_signaled: bool,
    );
}

const DEFAULT_RIGHTS: zx_rights_t = ZX_DEFAULT_INTERRUPT_RIGHTS;

/// The interrupt is virtual.
pub const INTERRUPT_VIRTUAL: u32 = 1 << 0;
/// The interrupt should be unmasked before waiting on the event.
///
/// Mutually exclusive with [`INTERRUPT_UNMASK_PREWAIT_UNLOCKED`].
pub const INTERRUPT_UNMASK_PREWAIT: u32 = 1 << 1;
/// The same as [`INTERRUPT_UNMASK_PREWAIT`] except release the dispatcher
/// spinlock before waiting.
///
/// Mutually exclusive with [`INTERRUPT_UNMASK_PREWAIT`].
const INTERRUPT_UNMASK_PREWAIT_UNLOCKED: u32 = 1 << 2;
/// The interrupt should be masked following waiting.
pub const INTERRUPT_MASK_POSTWAIT: u32 = 1 << 4;
/// The interrupt may wake the system from suspend.
pub const INTERRUPT_WAKE_VECTOR: u32 = 1 << 5;
/// Allow kernel tests to call `ack()` without binding to a port.
pub const INTERRUPT_ALLOW_ACK_WITHOUT_PORT_FOR_TEST: u32 = 1 << 6;
/// The interrupt should use monotonic timestamps instead of the default,
/// which is boot timestamps.
pub const INTERRUPT_TIMESTAMP_MONO: u32 = 1 << 7;

/// Internal state of an [`InterruptDispatcher`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptState {
    /// A thread is blocked waiting for the interrupt.
    Waiting = 0,
    /// The interrupt dispatcher has been destroyed.
    Destroyed = 1,
    /// The interrupt has been triggered and is pending delivery.
    Triggered = 2,
    /// The interrupt has been delivered and is waiting for an acknowledgment.
    NeedAck = 3,
    /// The interrupt is idle.
    Idle = 4,
}

/// Post-acknowledgment state of an [`InterruptDispatcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostAckState {
    /// The interrupt was acked, and there was no other interrupt pending afterwards.
    FullyAcked,
    /// The interrupt was acked, but immediately re-triggered due to a pending irq.
    Retriggered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckBehavior {
    ClearSignaled,
    RemainSignaled,
}

/// Diagnostics information for a wake vector interrupt.
#[repr(C)]
pub struct WakeVectorDiagnostics {
    /// Whether the wake vector is currently enabled.
    pub enabled: bool,
    /// Kernel object ID of the interrupt dispatcher.
    pub koid: zx_koid_t,
    /// NUL-terminated extra diagnostic string.
    pub extra: [u8; ZX_MAX_NAME_LEN],
}

impl WakeVectorDiagnostics {
    /// Formats `args` as a NUL-terminated string into `self.extra`.
    pub fn print_extra(&mut self, args: core::fmt::Arguments<'_>) {
        struct SliceWriter<'a> {
            buf: &'a mut [u8],
            pos: usize,
        }
        impl core::fmt::Write for SliceWriter<'_> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let bytes = s.as_bytes();
                let avail = self.buf.len().saturating_sub(1).saturating_sub(self.pos);
                let len = bytes.len().min(avail);
                self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
                self.pos += len;
                self.buf[self.pos] = 0;
                Ok(())
            }
        }
        let mut w = SliceWriter { buf: &mut self.extra, pos: 0 };
        let _ = core::fmt::Write::write_fmt(&mut w, args);
    }
}

struct WakeEventGuard;

impl WakeEventGuard {
    #[inline]
    fn trigger(&mut self, disp: &InterruptDispatcher, timestamp: zx_instant_boot_t) {
        // SAFETY: `self` is `#[guarded_by(spinlock)]` in `InterruptDispatcherState`, so holding
        // `&mut self` proves the caller holds `spinlock` (`RawSpinlock` with `IrqSavePolicy`),
        // satisfying `TA_GUARDED(spinlock_)` for `wake_event_.Trigger(...)`.
        unsafe {
            cpp_interrupt_dispatcher_wake_event_trigger(disp, timestamp);
        }
    }

    #[inline]
    fn acknowledge(&mut self, disp: &InterruptDispatcher, behavior: AckBehavior) {
        let remain_signaled = matches!(behavior, AckBehavior::RemainSignaled);
        // SAFETY: `self` is `#[guarded_by(spinlock)]` in `InterruptDispatcherState`, so holding
        // `&mut self` proves the caller holds `spinlock` (`RawSpinlock` with `IrqSavePolicy`),
        // satisfying `TA_GUARDED(spinlock_)` for `wake_event_.Acknowledge(...)`.
        unsafe {
            cpp_interrupt_dispatcher_wake_event_acknowledge(disp, remain_signaled);
        }
    }
}

/// Concrete interrupt dispatcher variant stored inside [`InterruptDispatcherState`].
#[repr(C)]
pub enum InterruptKind {
    /// Virtual interrupt dispatcher.
    Virtual(VirtualInterruptDispatcher),
    /// Hardware interrupt event dispatcher.
    Event(InterruptEventDispatcher),
    /// MSI / MSI-X interrupt dispatcher.
    Msi(MsiInterruptDispatcher),
}

zr::static_assert_size_and_align!(InterruptKind, 80, 8);

impl InterruptKind {
    #[inline]
    fn mask_interrupt(&self) {
        match self {
            Self::Virtual(_) => {}
            Self::Event(ev) => ev.mask_interrupt(),
            Self::Msi(msi) => msi.mask_interrupt(),
        }
    }

    #[inline]
    fn unmask_interrupt(&self) {
        match self {
            Self::Virtual(_) => {}
            Self::Event(ev) => ev.unmask_interrupt(),
            Self::Msi(msi) => msi.unmask_interrupt(),
        }
    }

    #[inline]
    fn deactivate_interrupt(&self) {
        match self {
            Self::Virtual(_) | Self::Msi(_) => {}
            Self::Event(ev) => ev.deactivate_interrupt(),
        }
    }

    #[inline]
    fn unregister_interrupt_handler(&self) {
        match self {
            Self::Virtual(_) => {}
            Self::Event(ev) => ev.unregister_interrupt_handler(),
            Self::Msi(msi) => msi.unregister_interrupt_handler(),
        }
    }
}

zr::static_assert_size_and_align!(
    InterruptDispatcherState,
    object_constants::kInterruptDispatcherStateSize,
    object_constants::kInterruptDispatcherStateAlign,
);

/// Rust state embedded inside C++ `InterruptDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct InterruptDispatcherState {
    canary: Canary<{ fbl::magic(b"INTD") }>,
    flags: u32,
    options: u32,
    dispatcher: NonNull<InterruptDispatcher>,

    #[pin]
    event: AutounsignalEvent,

    kind: UnsafeCell<MaybeUninit<InterruptKind>>,

    #[guarded_by(spinlock)]
    wake_event: WakeEventGuard,

    #[guarded_by(spinlock)]
    timestamp: zx_time_t,

    // Current state of the interrupt object
    #[guarded_by(spinlock)]
    state: InterruptState,

    #[pin]
    port_packet: PortInterruptPacket,

    #[guarded_by(spinlock)]
    port_dispatcher: Option<RefPtr<PortDispatcher>>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    // Controls the access to Interrupt properties
    #[mutex]
    spinlock: KMutex<RawSpinlock>,
}

#[pinned_drop]
impl PinnedDrop for InterruptDispatcherState {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `self.kind` was initialized during `rust_interrupt_dispatcher_state_init`
        // and must be dropped first (matching C++ derived-before-base destructor order)
        // while all `InterruptDispatcherState` fields (including `canary` and `flags`) and
        // C++ `InterruptDispatcher` members (`wake_event_`) are still alive.
        unsafe {
            (*self.kind.get()).assume_init_drop();
        }
    }
}

// SAFETY: `InterruptDispatcherState` synchronizes all mutable fields via `lock` and `spinlock`,
// and `dispatcher` points to the enclosing `InterruptDispatcher` which outlives `Self`.
unsafe impl Send for InterruptDispatcherState {}
// SAFETY: Shared references are synchronized via `lock` and `spinlock`.
unsafe impl Sync for InterruptDispatcherState {}

impl InterruptDispatcherState {
    /// Creates a pin-initializer for `InterruptDispatcherState`.
    pub fn new(
        dispatcher: NonNull<InterruptDispatcher>,
        flags: u32,
        options: u32,
    ) -> impl PinInit<Self, Infallible> {
        // It is an error to specify both INTERRUPT_UNMASK_PREWAIT and
        // INTERRUPT_UNMASK_PREWAIT_UNLOCKED.
        debug_assert!(
            (flags & INTERRUPT_UNMASK_PREWAIT) == 0
                || (flags & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) == 0
        );
        pin_init!(Self {
            canary: Canary::new(),
            flags,
            options,
            dispatcher,
            event <- AutounsignalEvent::init(false),
            kind: UnsafeCell::new(MaybeUninit::uninit()),
            wake_event: WakeEventGuard.into(),
            timestamp: 0.into(),
            state: InterruptState::Idle.into(),
            port_packet <- PortInterruptPacket::init(),
            port_dispatcher: None.into(),
            lock <- KMutex::init(),
            spinlock <- KMutex::init(),
        })
    }

    /// Returns `true` if this interrupt is a virtual interrupt.
    #[inline]
    pub fn is_virtual(&self) -> bool {
        matches!(self.kind(), InterruptKind::Virtual(_))
    }

    #[inline]
    fn as_virtual(&self) -> Option<&VirtualInterruptDispatcher> {
        match self.kind() {
            InterruptKind::Virtual(v) => Some(v),
            _ => None,
        }
    }

    /// Returns a reference to the initialized [`InterruptKind`] variant.
    #[inline]
    pub fn kind(&self) -> &InterruptKind {
        // SAFETY: `self.kind` is initialized during `rust_interrupt_dispatcher_state_init`
        // before any method accesses `kind()`, and remains initialized until `PinnedDrop::drop`.
        unsafe { (*self.kind.get()).assume_init_ref() }
    }

    fn wait_for_interrupt(&self, disp: &InterruptDispatcher) -> Result<zx_time_t, Status> {
        loop {
            if let Some(opt_status) = disp.begin_wait_for_interrupt() {
                return opt_status;
            }

            disp.do_wait_for_interrupt_block()?;
        }
    }

    /// Triggers a virtual interrupt at `timestamp`.
    pub fn trigger(&self, disp: &InterruptDispatcher, timestamp: zx_time_t) -> Result<(), Status> {
        if (self.flags & INTERRUPT_VIRTUAL) == 0 {
            return Err(Status::BAD_STATE);
        }

        // Use preempt disable for correctness to prevent rescheduling when waking a
        // thread while holding the spinlock.
        let _preempt_disable = AutoPreemptDisabler::new();
        lock!(let mut guard = self.lock_spinlock());
        let mut fields = guard.as_mut().fields_mut();

        // Nothing to do if this interrupt has been destroyed and is waiting to be
        // cleaned up.
        if *fields.state == InterruptState::Destroyed {
            return Err(Status::CANCELED);
        }

        // only record timestamp if this is the first signal since we started waiting
        if *fields.timestamp == 0 {
            *fields.timestamp = timestamp;
        }

        if disp.is_wake_vector() {
            // If this interrupt is configured to use Monotonic timestamps, then we need
            // to capture a new boot timestamp to use as the trigger time for the wake
            // vector. There is no way (nor will there ever be a way) to convert (after
            // initial capture) from monotonic time to boot time, or vice versa.
            let boot_time_trigger = if (self.options & INTERRUPT_TIMESTAMP_MONO) == 0 {
                timestamp
            } else {
                current_boot_time().0
            };
            fields.wake_event.trigger(disp, boot_time_trigger);
        }

        if *fields.state == InterruptState::NeedAck && fields.port_dispatcher.is_some() {
            // Cannot trigger a interrupt without ACK
            // only record timestamp if this is the first signal since we started waiting
            return Ok(());
        }

        if fields.port_dispatcher.is_some() {
            // Only send a packet if we are not already in the NeedAck state. If we are
            // already in NeedAck, the packet will be sent as soon as the user
            // explicitly acks the interrupt.
            if *fields.state != InterruptState::NeedAck {
                InterruptDispatcher::send_packet_locked(self, &mut fields, timestamp);
                *fields.state = InterruptState::NeedAck;
            }
        } else {
            disp.signal();

            // Do not change state to Triggered if we are in the NeedAck state. We
            // recorded a timestamp (above) which is the signal to a calling thread that
            // we are in the signaled state, and as soon as a thread blocks on the
            // object again, we will deliver the interrupt to them.
            if *fields.state != InterruptState::NeedAck {
                *fields.state = InterruptState::Triggered;
            }
        }
        Ok(())
    }

    /// Destroys the interrupt state, masking and unregistering any underlying IRQ.
    pub fn destroy(&self, disp: &InterruptDispatcher) -> Result<(), Status> {
        let old_state = {
            lock!(let mut guard = self.lock_spinlock());
            let fields = guard.as_mut().fields_mut();
            core::mem::replace(fields.state, InterruptState::Destroyed)
        };

        if old_state == InterruptState::Destroyed {
            return Ok(());
        }

        // The interrupt may presently have been fired and we could already be about to acquire the
        // spinlock in interrupt_handler. If we were to call unregister_interrupt_handler whilst
        // holding the spinlock then we risk a deadlock scenario where the platform interrupt code
        // may have taken a lock to call interrupt_handler, and it might take the same lock when we
        // call unregister_interrupt_handler.

        self.kind().mask_interrupt();
        self.kind().deactivate_interrupt();
        self.kind().unregister_interrupt_handler();

        // Use preempt disable for correctness to prevent rescheduling when waking a
        // thread while holding the spinlock.
        let _preempt_disable = AutoPreemptDisabler::new();
        lock!(let mut guard = self.lock_spinlock());
        let fields = guard.as_mut().fields_mut();

        if let Some(port_dispatcher) = fields.port_dispatcher.as_ref() {
            let packet_was_in_queue = port_dispatcher.remove_interrupt_packet(&self.port_packet);
            if old_state == InterruptState::NeedAck && !packet_was_in_queue {
                return Err(Status::NOT_FOUND);
            }
            if old_state == InterruptState::Idle
                || (old_state == InterruptState::NeedAck && packet_was_in_queue)
            {
                return Ok(());
            }
        } else {
            disp.signal();
        }
        Ok(())
    }

    fn ack(&self, disp: &InterruptDispatcher) -> Result<(), Status> {
        let res = disp.ack_internal();
        res.map(|_| ())
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher for hardware, MSI, and virtual interrupts.
    pub struct InterruptDispatcher,
    InterruptDispatcherState,
    ZX_OBJ_TYPE_INTERRUPT,
    object_constants::kInterruptDispatcherStateOffset
);

impl InterruptDispatcher {
    /// Returns the default handle rights for an `InterruptDispatcher`.
    pub const fn default_rights() -> zx_rights_t {
        DEFAULT_RIGHTS
    }

    /// Initializes the `InterruptKind` of this `InterruptDispatcher` during construction.
    ///
    /// # Safety
    ///
    /// Must be called exactly once during `init_kind` in `InterruptDispatcher::create_raw`
    /// after `InterruptDispatcherState` has been initialized and before any method accesses
    /// `kind()`.
    #[inline]
    pub unsafe fn init_kind(&self, kind: InterruptKind) {
        // SAFETY: Guaranteed by the caller to be called once during single-threaded
        // construction before `kind()` is accessed.
        unsafe {
            (*self.state().kind.get()).write(kind);
        }
    }

    /// Allocates and initializes a new `InterruptDispatcher` using `init_kind` to populate its
    /// [`InterruptKind`].
    pub fn create_raw<F>(flags: u32, options: u32, init_kind: F) -> Option<RefPtr<Self>>
    where
        F: FnOnce(&Self),
    {
        /// # Safety
        ///
        /// `disp` must be a valid reference to an `InterruptDispatcher` with initialized
        /// `InterruptDispatcherState`, and `ctx` must point to a valid `Option<F>`.
        unsafe extern "C" fn trampoline<F>(disp: &InterruptDispatcher, ctx: *mut u8)
        where
            F: FnOnce(&InterruptDispatcher),
        {
            // SAFETY: `ctx` is valid per caller contract.
            let f = unsafe { (&mut *(ctx as *mut Option<F>)).take().unwrap() };
            f(disp);
        }
        let mut init_opt = Some(init_kind);
        let mut raw_disp: *mut Self = core::ptr::null_mut();
        // SAFETY: `init_opt` is valid on the stack for the duration of
        // `cpp_interrupt_dispatcher_create`, and `raw_disp` is a valid out-pointer.
        let status = unsafe {
            cpp_interrupt_dispatcher_create(
                flags,
                options,
                (&mut init_opt as *mut Option<F>).cast(),
                trampoline::<F>,
                &mut raw_disp,
            )
        };
        Status::ok(status).ok()?;
        // SAFETY: `raw_disp` is a valid exported `RefPtr<InterruptDispatcher>` from C++.
        Some(unsafe { RefPtr::from_raw(raw_disp) })
    }

    /// Returns `true` if this interrupt is configured as a wake vector.
    #[inline]
    pub fn is_wake_vector(&self) -> bool {
        (self.state().flags & INTERRUPT_WAKE_VECTOR) != 0
    }

    #[inline]
    fn signal(&self) {
        self.state().event.signal();
    }

    // Allow subclasses to add/remove the wake event instance from the global diagnostics list at
    // the appropriate times during initialization and teardown. These methods do not need to be
    // called unless the subclass needs to be able to wake the system from suspend. See
    // lib/wake-vector.h for the specific requirements regarding when to initialize and destroy the
    // wake event.
    /// Initializes the embedded `WakeEvent` and registers it in the global wake-vector list.
    #[inline]
    pub fn initialize_wake_event(&self) {
        // SAFETY: `self` is a valid `InterruptDispatcher`.
        unsafe {
            cpp_interrupt_dispatcher_wake_event_initialize(self);
        }
    }

    /// Destroys the embedded `WakeEvent` and removes it from the global wake-vector list.
    #[inline]
    pub fn destroy_wake_event(&self) {
        // SAFETY: `self` is a valid `InterruptDispatcher`.
        unsafe {
            cpp_interrupt_dispatcher_wake_event_destroy(self);
        }
    }

    /// Populates wake vector diagnostics for this interrupt dispatcher.
    pub fn get_diagnostics(&self, diagnostics_out: &mut WakeVectorDiagnostics) {
        match self.state().kind() {
            InterruptKind::Virtual(v) => v.get_diagnostics(diagnostics_out),
            InterruptKind::Event(ev) => ev.get_diagnostics(diagnostics_out),
            InterruptKind::Msi(msi) => msi.get_diagnostics(diagnostics_out),
        }
    }

    /// Returns information about this interrupt in a `zx_object_get_info` call.
    pub fn get_info(&self) -> zx_info_interrupt_t {
        zx_info_interrupt_t { options: self.state().options }
    }

    /// Blocks the calling thread until this interrupt triggers or is destroyed.
    pub fn wait_for_interrupt(&self) -> Result<zx_time_t, Status> {
        self.state()
            .as_virtual()
            .map_or_else(|| self.state().wait_for_interrupt(self), |v| v.wait_for_interrupt(self))
    }

    // The zx_interrupt_wait operation is broken down into two phases. First, we
    // begin_wait_for_interrupt, which will handle internal bookkeeping and either return a status
    // code if the wait operation is finished (success or failure), or it will return nothing if we
    // need to block and try again later.
    //
    // Afterwards, if we need to block, we will call do_wait_for_interrupt_block and try again if
    // the block operation succeeds.
    /// Performs the first phase of waiting for an interrupt.
    pub fn begin_wait_for_interrupt(&self) -> Option<Result<zx_time_t, Status>> {
        let state = self.state();
        let mut defer_unmask = false;

        {
            // Disable preemption to ensure we don't trigger a reschedule while holding this
            // spinlock.
            let _preempt_disable = AutoPreemptDisabler::new();
            lock!(let mut guard = state.lock_spinlock());
            let fields = guard.as_mut().fields_mut();
            if fields.port_dispatcher.is_some() {
                return Some(Err(Status::BAD_STATE));
            }
            match *fields.state {
                InterruptState::Destroyed => {
                    return Some(Err(Status::CANCELED));
                }
                InterruptState::Triggered => {
                    *fields.state = InterruptState::NeedAck;
                    let out_timestamp = *fields.timestamp;
                    *fields.timestamp = 0;
                    return Some(state.event.unsignal().map(|()| out_timestamp));
                }
                InterruptState::NeedAck => {
                    // We are in the NeedAck state and have been waiting for a thread to
                    // block on this object to serve as our Ack signal. _If_ we are an
                    // edge triggered interrupt, it is possible that we were signaled (by
                    // hardware) once again after unblocking the time before. If that has
                    // happened, we will have a non-zero value stored in timestamp, meaning
                    // that there is another IRQ pending.
                    //
                    // So, if there is a pending IRQ, consume the timestamp, clear the
                    // signal, and do not block our thread (as if we were in the Triggered
                    // state). Otherwise, unmask our interrupt at the proper point in the
                    // sequence, change to the waiting state, and block our thread.
                    if *fields.timestamp != 0 {
                        // if we are a wake vector and our consuming a pending interrupt, then
                        // make sure that our wake event remains signaled as we record the
                        // acknowledgement event.
                        if self.is_wake_vector() {
                            fields.wake_event.acknowledge(self, AckBehavior::RemainSignaled);
                        }

                        let out_timestamp = *fields.timestamp;
                        *fields.timestamp = 0;
                        return Some(state.event.unsignal().map(|()| out_timestamp));
                    } else {
                        // There is no pending interrupt. If we are a wake vector, ack our wake
                        // event and clear its signaled state.
                        if self.is_wake_vector() {
                            fields.wake_event.acknowledge(self, AckBehavior::ClearSignaled);
                        }
                    }

                    if (state.flags & INTERRUPT_UNMASK_PREWAIT) != 0 {
                        state.kind().unmask_interrupt();
                    } else if (state.flags & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) != 0 {
                        defer_unmask = true;
                    }
                }
                InterruptState::Idle => {}
                InterruptState::Waiting => {
                    return Some(Err(Status::BAD_STATE));
                }
            }
            *fields.state = InterruptState::Waiting;
        }

        if defer_unmask {
            state.kind().unmask_interrupt();
        }

        None
    }

    /// Blocks on the internal event during `wait_for_interrupt`.
    pub fn do_wait_for_interrupt_block(&self) -> Result<(), Status> {
        let state = self.state();
        let _by = AutoBlocked::new(Blocked::INTERRUPT);
        let status = state.event.wait(&Deadline::infinite());
        if status.is_err() {
            // The Event::Wait call was interrupted and we need to retry
            // but before we retry we will set the interrupt state
            // back to Idle if we are still in the Waiting state.
            lock!(let mut guard = state.lock_spinlock());
            let fields = guard.as_mut().fields_mut();
            if *fields.state == InterruptState::Waiting {
                *fields.state = InterruptState::Idle;
            }
        }
        status
    }

    fn send_packet_locked(
        state: &InterruptDispatcherState,
        fields: &mut InterruptDispatcherStateSpinlockFieldsMut<'_>,
        timestamp: zx_time_t,
    ) -> bool {
        let port_dispatcher =
            fields.port_dispatcher.as_ref().expect("port_dispatcher must be Some");
        let status = port_dispatcher.queue_interrupt_packet(&state.port_packet, timestamp);
        if (state.flags & INTERRUPT_MASK_POSTWAIT) != 0 {
            state.kind().mask_interrupt();
        }
        *fields.timestamp = 0;
        status
    }

    /// Triggers a virtual interrupt at `timestamp`.
    pub fn trigger(&self, timestamp: zx_time_t) -> Result<(), Status> {
        self.state()
            .as_virtual()
            .map_or_else(|| self.state().trigger(self, timestamp), |v| v.trigger(self, timestamp))
    }

    /// Handles a hardware or MSI interrupt in interrupt context.
    pub fn interrupt_handler(&self) {
        let state = self.state();
        // Using preempt disable is not necessary for correctness, since we should
        // be in an interrupt context with preemption disabled, but we re-disable anyway
        // for clarity and robustness.
        let _preempt_disable = AutoPreemptDisabler::new();
        lock!(let mut guard = state.lock_spinlock());
        let mut fields = guard.as_mut().fields_mut();

        if *fields.state == InterruptState::Destroyed {
            return;
        }

        let trigger_time: CurrentTicksObservation = timer_current_mono_and_boot_ticks();
        let mut boot_trigger_time: Option<zx_instant_boot_t> = None;

        // only record timestamp if this is the first IRQ since we started waiting
        if *fields.timestamp == 0 {
            if (state.flags & INTERRUPT_TIMESTAMP_MONO) != 0 {
                *fields.timestamp =
                    timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(trigger_time.mono_now);
            } else {
                let scaled =
                    timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(trigger_time.boot_now);
                *fields.timestamp = scaled;
                boot_trigger_time = Some(scaled);
            }
        }

        // If we are a wake vector, trigger the wake event which will wake the system
        // if suspended and prevent entering suspend until acknowledged.
        if self.is_wake_vector() {
            fields.wake_event.trigger(
                self,
                boot_trigger_time.unwrap_or_else(|| {
                    timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(trigger_time.boot_now)
                }),
            );
        }

        if fields.port_dispatcher.is_some() {
            // Only send a packet if we are not already in the NeedAck state. If we are
            // already in NeedAck, the packet will be sent as soon as the user
            // explicitly acks the interrupt.
            if *fields.state != InterruptState::NeedAck {
                let ts = *fields.timestamp;
                Self::send_packet_locked(state, &mut fields, ts);
                *fields.state = InterruptState::NeedAck;
            }
        } else {
            if (state.flags & INTERRUPT_MASK_POSTWAIT) != 0 {
                state.kind().mask_interrupt();
            }
            self.signal();

            // Do not change state to Triggered if we are in the NeedAck state. We
            // recorded a timestamp (above) which is the signal to a calling thread that
            // we are in the signaled state, and as soon as a thread blocks on the
            // object again, we will deliver the interrupt to them.
            if *fields.state != InterruptState::NeedAck {
                *fields.state = InterruptState::Triggered;
            }
        }
    }

    /// Destroys this interrupt dispatcher, masking and unregistering any underlying IRQ.
    pub fn destroy(&self) -> Result<(), Status> {
        self.state().as_virtual().map_or_else(|| self.state().destroy(self), |v| v.destroy(self))
    }

    /// Binds this interrupt to `port_dispatcher` with `key`.
    pub fn bind(&self, port_dispatcher: RefPtr<PortDispatcher>, key: u64) -> Result<(), Status> {
        let state = self.state();
        let _preempt_disable = AutoPreemptDisabler::new();
        lock!(let mut guard = state.lock_spinlock());
        let mut fields = guard.as_mut().fields_mut();
        if *fields.state == InterruptState::Destroyed {
            return Err(Status::CANCELED);
        }
        if *fields.state == InterruptState::Waiting {
            return Err(Status::BAD_STATE);
        }
        if fields.port_dispatcher.is_some() {
            return Err(Status::ALREADY_BOUND);
        }

        // If an interrupt is bound to a port there is a conflict between UNMASK_PREWAIT_UNLOCKED
        // and MASK_POSTWAIT because the mask operation will by necessity happen before leaving the
        // dispatcher spinlock, leading to a mask operation immediately followed by the deferred
        // unmask operation.
        if (state.flags & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) != 0
            && (state.flags & INTERRUPT_MASK_POSTWAIT) != 0
        {
            return Err(Status::INVALID_ARGS);
        }

        *fields.port_dispatcher = Some(port_dispatcher);
        // SAFETY: `state.lock_spinlock()` is held and `port_packet` is not queued in any port
        // because `fields.port_dispatcher` was `None`.
        unsafe {
            state.port_packet.set_key(key);
        }

        if *fields.state == InterruptState::Triggered {
            let ts = *fields.timestamp;
            Self::send_packet_locked(state, &mut fields, ts);
            *fields.state = InterruptState::NeedAck;
        }

        Ok(())
    }

    /// Unbinds this interrupt from `port_dispatcher`.
    pub fn unbind(&self, port_dispatcher: &RefPtr<PortDispatcher>) -> Result<(), Status> {
        let state = self.state();
        // Moving port_dispatcher to the local variable ensures it will not be destroyed while
        // holding this spinlock.
        let _dispatcher: Option<RefPtr<PortDispatcher>>;
        {
            lock!(let mut guard = state.lock_spinlock());
            let fields = guard.as_mut().fields_mut();

            if !fields
                .port_dispatcher
                .as_ref()
                .is_some_and(|bound| RefPtr::ptr_eq(bound, port_dispatcher))
            {
                // This case also covers the HasVcpu() case.
                return Err(Status::NOT_FOUND);
            }
            if *fields.state == InterruptState::Destroyed {
                return Err(Status::CANCELED);
            }
            // Remove the packet for this interrupt from this port on an unbind before actually
            // doing the unbind. This protects against the case where the interrupt dispatcher
            // goes away between an unbind and a port_wait.
            let removed_packet = fields
                .port_dispatcher
                .as_ref()
                .unwrap()
                .remove_interrupt_packet(&state.port_packet);
            // SAFETY: `state.lock_spinlock()` is held and `remove_interrupt_packet` has guaranteed
            // `port_packet` is no longer queued in `port_dispatcher`.
            unsafe {
                state.port_packet.set_key(0);
            }
            _dispatcher = fields.port_dispatcher.take();

            // If we were in the NeedAck state, and we successfully removed the packet
            // from the port, then revert back to the Triggered state. No one has
            // managed to process this interrupt yet, and we don't want to lose it.
            // Someone can either wait on this interrupt, or re-bind it to a new port,
            // and they should still be able to process the interrupt which has not
            // _technically_ been delivered yet.
            //
            // If the packet has already been read, then just stay in the NeedAck state.
            // Someone has already processed the packet.
            if *fields.state == InterruptState::NeedAck && removed_packet {
                *fields.state = InterruptState::Triggered;
                // SAFETY: `state.lock_spinlock()` is held and `port_packet` was just removed from
                // `port_dispatcher`.
                unsafe {
                    *fields.timestamp = state.port_packet.timestamp();
                    state.port_packet.set_key(0);
                    state.port_packet.set_timestamp(0);
                }
            }
        }
        Ok(())
    }

    /// Acknowledges this interrupt and unmasks or retriggers as appropriate.
    pub fn ack(&self) -> Result<(), Status> {
        self.state().as_virtual().map_or_else(|| self.state().ack(self), |v| v.ack(self))
    }

    /// Performs the lower-level acknowledgment bookkeeping and returns [`PostAckState`].
    pub fn ack_internal(&self) -> Result<PostAckState, Status> {
        let state = self.state();
        let mut post_ack_state = PostAckState::FullyAcked;
        let mut defer_unmask = false;
        // Use preempt disable to reduce the likelihood of the woken thread running
        // while the spinlock is still held.
        let _preempt_disable = AutoPreemptDisabler::new();
        {
            lock!(let mut guard = state.lock_spinlock());
            let mut fields = guard.as_mut().fields_mut();
            if fields.port_dispatcher.is_none()
                && (state.flags & INTERRUPT_ALLOW_ACK_WITHOUT_PORT_FOR_TEST) == 0
            {
                return Err(Status::BAD_STATE);
            }

            if *fields.state == InterruptState::Destroyed {
                return Err(Status::CANCELED);
            }

            if *fields.state == InterruptState::NeedAck {
                if (state.flags & INTERRUPT_UNMASK_PREWAIT) != 0 {
                    state.kind().unmask_interrupt();
                } else if (state.flags & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) != 0 {
                    defer_unmask = true;
                }

                if *fields.timestamp != 0 {
                    let ts = *fields.timestamp;
                    if !Self::send_packet_locked(state, &mut fields, ts) {
                        // We cannot queue another packet here.
                        // If we reach here it means that the
                        // interrupt packet has not been processed,
                        // another interrupt has occurred & then the
                        // interrupt was ACK'd
                        return Err(Status::BAD_STATE);
                    }

                    // If we are a wake vector, record our last_ack timestamp, but do not
                    // clear the signaled state. We just sent a new port packet, so there
                    // is another interrupt signal on its way to user mode.
                    if self.is_wake_vector() {
                        fields.wake_event.acknowledge(self, AckBehavior::RemainSignaled);
                    }

                    post_ack_state = PostAckState::Retriggered;
                } else {
                    // There are no other interrupt pending right now. If we are a wake
                    // vector, ack our wake event clearing the signaled state in the
                    // process, then return to the Idle state.
                    if self.is_wake_vector() {
                        fields.wake_event.acknowledge(self, AckBehavior::ClearSignaled);
                    }
                    *fields.state = InterruptState::Idle;
                }
            }
        }

        if defer_unmask {
            state.kind().unmask_interrupt();
        }
        Ok(post_ack_state)
    }

    /// Invoked when the last user handle to this interrupt dispatcher is closed.
    pub fn on_zero_handles(&self) {
        let _ = self.destroy();
    }
}
