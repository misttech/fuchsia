// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::user_copy::UserOutPtr;
use debug::dprintf;
use fbl::Canary;
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_SAMPLER_RIGHTS, ZX_OBJ_TYPE_SAMPLER, zx_rights_t, zx_sampler_config_t};

use super::KernelHandle;
use super::sampler_dispatcher_ffi::cpp_sampler_dispatcher_create;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    SamplerDispatcherState,
    object_constants::kSamplerDispatcherStateSize,
    object_constants::kSamplerDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_SAMPLER_CREATE_COUNT, "dispatcher.sampler.create", Sum);
define_kcounter!(DISPATCHER_SAMPLER_DESTROY_COUNT, "dispatcher.sampler.destroy", Sum);

/// Internal state storage for `SamplerDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct SamplerDispatcherState {
    canary: Canary<{ fbl::magic(b"SAMP") }>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl SamplerDispatcherState {
    /// Initializes the `SamplerDispatcherState`.
    pub fn init(
        _dispatcher: *const SamplerDispatcher,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_SAMPLER_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for SamplerDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_SAMPLER_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct SamplerDispatcher,
    SamplerDispatcherState,
    ZX_OBJ_TYPE_SAMPLER,
    object_constants::kSamplerDispatcherStateOffset
);

impl SamplerDispatcher {
    /// Returns the default rights for a SamplerDispatcher handle.
    pub fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_SAMPLER_RIGHTS
    }

    /// Creates a new `SamplerDispatcher` with the given configuration.
    pub fn create(
        config: &zx_sampler_config_t,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        crate::thread_sampler::THREAD_SAMPLER.set_up(config)?;
        let rights = Self::default_rights();
        // SAFETY: `KernelHandle::create` passes a valid, writable `MaybeUninit` pointer `out` to
        // the closure, and `cpp_sampler_dispatcher_create` initializes `out` with a newly
        // constructed `KernelHandle<SamplerDispatcher>` on `ZX_OK`.
        let handle = unsafe { KernelHandle::create(|out| cpp_sampler_dispatcher_create(out)) }
            .inspect_err(|_| {
                let _ = crate::thread_sampler::THREAD_SAMPLER.destroy();
            })?;
        Ok((handle, rights))
    }

    /// Starts sampling session for this sampler.
    pub fn start(&self) -> Result<(), Status> {
        crate::thread_sampler::THREAD_SAMPLER.start()
    }

    /// Stops sampling session for this sampler.
    pub fn stop(&self) -> Result<(), Status> {
        crate::thread_sampler::THREAD_SAMPLER.stop()
    }

    /// Read out the data contained in the sampler buffers into `ptr` return the number of bytes
    /// written. The Sampling state must be Stopped before calling this function.
    ///
    /// `len` _must_ be at least equal to the total size of the sampler buffers, which can be
    /// queried by passing a null `ptr`. In this case, no data will be written and the return value
    /// will be the required minimum size of the buffer to write to.
    pub fn read_user(&self, ptr: UserOutPtr<u8>, len: usize) -> (Result<(), Status>, usize) {
        crate::thread_sampler::THREAD_SAMPLER.read_user(ptr, len)
    }

    /// Stops sampling and destroys the sampler buffers when the last handle to this dispatcher is
    /// closed.
    pub fn on_zero_handles(&self) {
        if let Err(status) = crate::thread_sampler::THREAD_SAMPLER.destroy() {
            dprintf!(ALWAYS, "Failed to cleanly destroy sampler: {}\n", status.into_raw());
        }
    }
}
