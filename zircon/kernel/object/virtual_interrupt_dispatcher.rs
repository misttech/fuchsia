// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ptr::NonNull;
use ksync::lock;
use zx_status::Status;
use zx_types::{
    ZX_INTERRUPT_TIMESTAMP_MONO, ZX_INTERRUPT_VIRTUAL, ZX_INTERRUPT_WAKE_VECTOR,
    ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx_rights_t, zx_time_t,
};

use super::DispatcherOps;
use super::handle::KernelHandle;
use super::interrupt_dispatcher::{
    INTERRUPT_TIMESTAMP_MONO, INTERRUPT_VIRTUAL, INTERRUPT_WAKE_VECTOR, InterruptDispatcher,
    InterruptKind, PostAckState, WakeVectorDiagnostics,
};
use crate::counters::define_kcounter;

define_kcounter!(
    DISPATCHER_VIRTUAL_INTERRUPT_CREATE_COUNT,
    "dispatcher.virtual_interrupt.create",
    Sum
);
define_kcounter!(
    DISPATCHER_VIRTUAL_INTERRUPT_DESTROY_COUNT,
    "dispatcher.virtual_interrupt.destroy",
    Sum
);

/// Virtual interrupt dispatcher variant triggered via `zx_interrupt_trigger`.
#[repr(C)]
pub struct VirtualInterruptDispatcher {
    disp: NonNull<InterruptDispatcher>,
}

impl Drop for VirtualInterruptDispatcher {
    fn drop(&mut self) {
        // SAFETY: `self.disp` points to the enclosing `InterruptDispatcher`, which is valid
        // during `~InterruptDispatcher`.
        let disp = unsafe { self.disp.as_ref() };
        if disp.is_wake_vector() {
            disp.destroy_wake_event();
        }
        DISPATCHER_VIRTUAL_INTERRUPT_DESTROY_COUNT.add(1);
    }
}

impl VirtualInterruptDispatcher {
    /// Creates a new virtual interrupt dispatcher with `options`.
    pub fn create(
        options: u32,
    ) -> Result<(KernelHandle<InterruptDispatcher>, zx_rights_t), Status> {
        if (options & ZX_INTERRUPT_VIRTUAL) == 0 {
            return Err(Status::INVALID_ARGS);
        }

        const PERMITTED_OPTIONS: u32 =
            ZX_INTERRUPT_VIRTUAL | ZX_INTERRUPT_TIMESTAMP_MONO | ZX_INTERRUPT_WAKE_VECTOR;
        if (options & !PERMITTED_OPTIONS) != 0 {
            return Err(Status::INVALID_ARGS);
        }

        let mut flags = INTERRUPT_VIRTUAL;
        if (options & ZX_INTERRUPT_TIMESTAMP_MONO) != 0 {
            flags |= INTERRUPT_TIMESTAMP_MONO;
        }

        if (options & ZX_INTERRUPT_WAKE_VECTOR) != 0 {
            flags |= INTERRUPT_WAKE_VECTOR;
        }

        // Attempt to construct the dispatcher.
        let disp = InterruptDispatcher::create_raw(flags, options, |disp| {
            Self::new(disp);
        })
        .ok_or(Status::NO_MEMORY)?;
        let new_handle = KernelHandle::new(disp);

        // Transfer control of the new dispatcher to the creator and we are done.
        Ok((new_handle, InterruptDispatcher::default_rights()))
    }

    #[allow(clippy::new_ret_no_self)]
    fn new(disp: &InterruptDispatcher) {
        // SAFETY: Called once during `InterruptDispatcher::create_raw` construction.
        unsafe {
            disp.init_kind(InterruptKind::Virtual(Self { disp: NonNull::from(disp) }));
        }
        // Immediately after construction, virtual interrupt objects are always in the
        // untriggered state. Make sure that our initial signal state reflects this.
        disp.update_state(0, ZX_VIRTUAL_INTERRUPT_UNTRIGGERED);
        DISPATCHER_VIRTUAL_INTERRUPT_CREATE_COUNT.add(1);
        if disp.is_wake_vector() {
            disp.initialize_wake_event();
        }
    }

    /// Populates wake vector diagnostics for this virtual interrupt.
    pub fn get_diagnostics(&self, diagnostics_out: &mut WakeVectorDiagnostics) {
        // SAFETY: `self.disp` points to the valid enclosing `InterruptDispatcher`.
        let disp = unsafe { self.disp.as_ref() };
        diagnostics_out.enabled = disp.is_wake_vector();
        diagnostics_out.koid = disp.get_koid();
        diagnostics_out.print_extra(format_args!("VirtIRQ {}", disp.get_koid()));
    }

    /// Blocks until the virtual interrupt is triggered or destroyed, updating the
    /// `ZX_VIRTUAL_INTERRUPT_UNTRIGGERED` signal.
    pub fn wait_for_interrupt(&self, disp: &InterruptDispatcher) -> Result<zx_time_t, Status> {
        loop {
            // Hold the dispatcher lock during the first phase, and if we decide that we
            // need to block, be sure that our UNTRIGGERED signal has been asserted.
            {
                lock!(let guard = disp.state().lock_lock());
                let opt_status = disp.begin_wait_for_interrupt();
                if let Some(val) = opt_status {
                    return val;
                }

                disp.update_state_locked(guard.token(), 0, ZX_VIRTUAL_INTERRUPT_UNTRIGGERED);
            }

            disp.do_wait_for_interrupt_block()?;
        }
    }

    /// Triggers the virtual interrupt at `timestamp`, clearing `ZX_VIRTUAL_INTERRUPT_UNTRIGGERED`.
    pub fn trigger(&self, disp: &InterruptDispatcher, timestamp: zx_time_t) -> Result<(), Status> {
        lock!(let dispatcher_guard = disp.state().lock_lock());
        let result = disp.state().trigger(disp, timestamp);

        // If the trigger operation succeeded, then we know that we must be in either
        // the triggered or need-ack state, and we should clear the UNTRIGGERED
        // signal.
        if result.is_ok() {
            disp.update_state_locked(dispatcher_guard.token(), ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, 0);
        }

        result
    }

    /// Acknowledges the virtual interrupt and updates `ZX_VIRTUAL_INTERRUPT_UNTRIGGERED`.
    pub fn ack(&self, disp: &InterruptDispatcher) -> Result<(), Status> {
        // Hold the dispatcher lock while we perform our lower level ack operation.
        // If this succeeds, make sure that the UNTRIGGERED signal is asserted.
        lock!(let guard = disp.state().lock_lock());
        let result = disp.ack_internal();

        // If something goes wrong with the ack, then our UNTRIGGERED signal state
        // should not have changed.
        let result = result?;

        // Things went well, we successfully acked the interrupt and should assert the
        // UNTRIGGERED signal. If, however, the interrupt immediately retriggered
        // (because it already had another pending signal), be sure to clear the
        // UNTRIGGERED signal after asserting it.
        disp.update_state_locked(guard.token(), 0, ZX_VIRTUAL_INTERRUPT_UNTRIGGERED);
        if result == PostAckState::Retriggered {
            disp.update_state_locked(guard.token(), ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, 0);
        } else {
            debug_assert!(result == PostAckState::FullyAcked);
        }

        Ok(())
    }

    /// Destroys the virtual interrupt and asserts `ZX_VIRTUAL_INTERRUPT_UNTRIGGERED`.
    pub fn destroy(&self, disp: &InterruptDispatcher) -> Result<(), Status> {
        // An interrupt which has been explicitly destroyed (using a call to
        // zx_interrupt_destroy) can no longer be in the triggered state. Make sure
        // that we assert the virtual interrupt's UNTRIGGERED signal to be sure that
        // we don't accidentally end up with waiters stuck waiting on a destroyed
        // interrupt.
        lock!(let guard = disp.state().lock_lock());
        let result = disp.state().destroy(disp);
        disp.update_state_locked(guard.token(), 0, ZX_VIRTUAL_INTERRUPT_UNTRIGGERED);
        result
    }
}
