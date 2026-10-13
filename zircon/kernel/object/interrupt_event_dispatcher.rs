// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::ptr::NonNull;
use debug::dprintf;
use zx_status::Status;
use zx_types::{
    ZX_INTERRUPT_MODE_DEFAULT, ZX_INTERRUPT_MODE_EDGE_HIGH, ZX_INTERRUPT_MODE_EDGE_LOW,
    ZX_INTERRUPT_MODE_LEVEL_HIGH, ZX_INTERRUPT_MODE_LEVEL_LOW, ZX_INTERRUPT_MODE_MASK,
    ZX_INTERRUPT_REMAP_IRQ, ZX_INTERRUPT_TIMESTAMP_MONO, ZX_INTERRUPT_WAKE_VECTOR, zx_rights_t,
    zx_status_t,
};

use super::handle::KernelHandle;
use super::interrupt_dispatcher::{
    INTERRUPT_ALLOW_ACK_WITHOUT_PORT_FOR_TEST, INTERRUPT_MASK_POSTWAIT, INTERRUPT_TIMESTAMP_MONO,
    INTERRUPT_UNMASK_PREWAIT, INTERRUPT_WAKE_VECTOR, InterruptDispatcher, InterruptKind,
    WakeVectorDiagnostics,
};
use crate::counters::define_kcounter;
#[cfg(target_arch = "aarch64")]
use crate::dev_interrupt::deactivate_interrupt;
use crate::dev_interrupt::{
    InterruptHandler, InterruptPolarity, InterruptTriggerMode, configure_interrupt,
    get_interrupt_config, is_valid_interrupt, mask_interrupt, register_int_handler,
    remap_interrupt, unmask_interrupt,
};

define_kcounter!(DISPATCHER_INTERRUPT_EVENT_CREATE_COUNT, "dispatcher.interrupt_event.create", Sum);
define_kcounter!(
    DISPATCHER_INTERRUPT_EVENT_DESTROY_COUNT,
    "dispatcher.interrupt_event.destroy",
    Sum
);

/// Hardware IRQ interrupt dispatcher variant.
#[repr(C)]
pub struct InterruptEventDispatcher {
    disp: NonNull<InterruptDispatcher>,
    vector: u32,
}

impl Drop for InterruptEventDispatcher {
    fn drop(&mut self) {
        DISPATCHER_INTERRUPT_EVENT_DESTROY_COUNT.add(1);
        // SAFETY: `self.disp` points to the enclosing `InterruptDispatcher`, which is valid
        // during `~InterruptDispatcher`.
        unsafe { self.disp.as_ref() }.destroy_wake_event();
    }
}

impl InterruptEventDispatcher {
    /// Creates a new hardware interrupt dispatcher for `vector` with `options`.
    pub fn create(
        mut vector: u32,
        options: u32,
        allow_ack_without_port_for_test: bool,
    ) -> Result<(KernelHandle<InterruptDispatcher>, zx_rights_t), Status> {
        if !is_valid_interrupt(vector, 0) {
            return Err(Status::INVALID_ARGS);
        }

        if (options
            & !(ZX_INTERRUPT_REMAP_IRQ
                | ZX_INTERRUPT_MODE_MASK
                | ZX_INTERRUPT_WAKE_VECTOR
                | ZX_INTERRUPT_TIMESTAMP_MONO))
            != 0
        {
            return Err(Status::INVALID_ARGS);
        }

        let mut default_mode = false;
        let mut tm = InterruptTriggerMode::Edge;
        let mut pol = InterruptPolarity::Low;
        match options & ZX_INTERRUPT_MODE_MASK {
            ZX_INTERRUPT_MODE_DEFAULT => {
                default_mode = true;
                if let Err(status) = get_interrupt_config(vector, &mut tm, &mut pol) {
                    dprintf!(
                        CRITICAL,
                        "Internal error fetching IRQ {}'s config ({})\n",
                        vector,
                        status.into_raw()
                    );
                    return Err(Status::INTERNAL);
                }
            }
            ZX_INTERRUPT_MODE_EDGE_LOW => {
                tm = InterruptTriggerMode::Edge;
                pol = InterruptPolarity::Low;
            }
            ZX_INTERRUPT_MODE_EDGE_HIGH => {
                tm = InterruptTriggerMode::Edge;
                pol = InterruptPolarity::High;
            }
            ZX_INTERRUPT_MODE_LEVEL_LOW => {
                tm = InterruptTriggerMode::Level;
                pol = InterruptPolarity::Low;
            }
            ZX_INTERRUPT_MODE_LEVEL_HIGH => {
                tm = InterruptTriggerMode::Level;
                pol = InterruptPolarity::High;
            }
            _ => return Err(Status::INVALID_ARGS),
        }

        let mut interrupt_flags = 0u32;

        if (options & ZX_INTERRUPT_TIMESTAMP_MONO) != 0 {
            interrupt_flags |= INTERRUPT_TIMESTAMP_MONO;
        }

        if tm == InterruptTriggerMode::Level {
            interrupt_flags |= INTERRUPT_UNMASK_PREWAIT | INTERRUPT_MASK_POSTWAIT;
        }

        if (options & ZX_INTERRUPT_WAKE_VECTOR) != 0 {
            interrupt_flags |= INTERRUPT_WAKE_VECTOR;
        }

        if allow_ack_without_port_for_test {
            interrupt_flags |= INTERRUPT_ALLOW_ACK_WITHOUT_PORT_FOR_TEST;
        }

        // Remap the vector if we have been asked to do so.
        if (options & ZX_INTERRUPT_REMAP_IRQ) != 0 {
            vector = remap_interrupt(vector);
        }

        // Attempt to construct the dispatcher.
        // Do not create a KernelHandle until all initialization has succeeded;
        // if an interrupt already exists on |vector| our on_zero_handles() would
        // tear down the existing interrupt when creation fails.
        let disp = InterruptDispatcher::create_raw(interrupt_flags, options, |disp| {
            Self::new(disp, vector);
        })
        .ok_or(Status::NO_MEMORY)?;

        if !default_mode {
            configure_interrupt(vector, tm, pol)?;
        }

        // Register the interrupt
        let InterruptKind::Event(ev) = disp.state().kind() else {
            unreachable!();
        };
        ev.register_interrupt_handler()?;

        // TODO(https://fxbug.dev/348668110): Revisit this logging. Still needed?
        if (options & ZX_INTERRUPT_WAKE_VECTOR) != 0 {
            dprintf!(
                INFO,
                "creating interrupt wake vector with vector {}, koid {}\n",
                vector,
                disp.get_koid()
            );
        }

        let _ = unmask_interrupt(vector);

        // Transfer control of the new dispatcher to the creator and we are done.
        let rights = InterruptDispatcher::default_rights();
        let handle = KernelHandle::new(disp);

        Ok((handle, rights))
    }

    // This override of WakeVector::GetDiagnostics (and the destructor of this class) is marked
    // final to prevent further overrides. Because this method cannot be overridden further, it is
    // safe for this class to initialize / destroy InterruptDispatcher::wake_event in the
    // constructor / destructor. See lib/wake-vector.h for more details.
    /// Populates wake vector diagnostics for this hardware interrupt.
    pub fn get_diagnostics(&self, diagnostics_out: &mut WakeVectorDiagnostics) {
        // SAFETY: `self.disp` points to the valid enclosing `InterruptDispatcher`.
        let disp = unsafe { self.disp.as_ref() };
        diagnostics_out.enabled = disp.is_wake_vector();
        diagnostics_out.koid = disp.get_koid();
        diagnostics_out.print_extra(format_args!("IRQ {}", self.vector));
    }

    #[allow(clippy::new_ret_no_self)]
    fn new(disp: &InterruptDispatcher, vector: u32) {
        // SAFETY: Called once during `InterruptDispatcher::create_raw` construction.
        unsafe {
            disp.init_kind(InterruptKind::Event(Self { disp: NonNull::from(disp), vector }));
        }
        DISPATCHER_INTERRUPT_EVENT_CREATE_COUNT.add(1);
        disp.initialize_wake_event();
    }

    /// Masks the hardware interrupt vector.
    pub fn mask_interrupt(&self) {
        let _ = mask_interrupt(self.vector);
    }

    /// Unmasks the hardware interrupt vector.
    pub fn unmask_interrupt(&self) {
        let _ = unmask_interrupt(self.vector);
    }

    /// Deactivates the hardware interrupt vector on AArch64.
    pub fn deactivate_interrupt(&self) {
        #[cfg(target_arch = "aarch64")]
        {
            // deactivate_interrupt only exist in arm64
            let _ = deactivate_interrupt(self.vector);
        }
    }

    fn register_interrupt_handler(&self) -> Result<(), Status> {
        // SAFETY: `self.disp` remains valid until `unregister_interrupt_handler` is called.
        unsafe {
            register_int_handler(
                self.vector,
                InterruptHandler {
                    cookie: self.disp.as_ptr().cast(),
                    r#fn: Some({
                        extern "C" fn handler(cookie: *mut c_void) {
                            // SAFETY: `cookie` is a valid `*const InterruptDispatcher`.
                            unsafe { &*(cookie as *const InterruptDispatcher) }.interrupt_handler();
                        }
                        handler
                    }),
                },
            )
        }
    }

    /// Unregisters the hardware interrupt handler for this vector.
    pub fn unregister_interrupt_handler(&self) {
        // SAFETY: Passing `InterruptHandler::DEFAULT` unregisters the handler for `self.vector`.
        let _ = unsafe { register_int_handler(self.vector, InterruptHandler::DEFAULT) };
    }
}

/// # Safety
///
/// `handle_out` and `rights_out` must point to valid `KernelHandle<InterruptDispatcher>` and
/// `zx_rights_t` locations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_event_dispatcher_create(
    handle_out: *mut KernelHandle<InterruptDispatcher>,
    rights_out: *mut zx_rights_t,
    vector: u32,
    options: u32,
    allow_ack_without_port_for_test: bool,
) -> zx_status_t {
    match InterruptEventDispatcher::create(vector, options, allow_ack_without_port_for_test) {
        Ok((handle, rights)) => {
            // SAFETY: `handle_out` and `rights_out` are valid pointers provided by C++.
            unsafe {
                *rights_out = rights;
                *handle_out = handle;
            }
            zx_status::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
