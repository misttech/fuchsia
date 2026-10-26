// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::InterruptDisableGuard;
use crate::counters::define_kcounter;
use crate::platform_rs::timer::{
    current_boot_time, current_mono_time, timer_current_boot_ticks, timer_current_mono_ticks,
    timer_get_ticks_to_time_ratio,
};
use crate::vm::physmap::{is_physmap_phys_addr, paddr_to_physmap};
use crate::vm::pmm::{ALLOC_FLAG_ANY, ALLOC_FLAG_CAN_WAIT};
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use affine::{Exact, Ratio, Round, Saturate, Transform};
use concurrent::{SeqLock, SeqLockPayload, WriteGuard};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use fbl::{Canary, Name, RefPtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
use page::SIZE as PAGE_SIZE;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::{
    ZX_CLOCK_ARGS_VERSION_MASK, ZX_CLOCK_OPT_AUTO_START, ZX_CLOCK_OPT_BOOT,
    ZX_CLOCK_OPT_CONTINUOUS, ZX_CLOCK_OPT_MAPPABLE, ZX_CLOCK_OPT_MONOTONIC, ZX_CLOCK_OPTS_ALL,
    ZX_CLOCK_STARTED, ZX_CLOCK_UNKNOWN_ERROR, ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID,
    ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID, ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID,
    ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, ZX_CLOCK_UPDATED, ZX_DEFAULT_CLOCK_RIGHTS,
    ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_CLOCK, ZX_RIGHT_MAP, zx_clock_create_args_v1_t,
    zx_clock_details_v1_t, zx_clock_transformation_t, zx_clock_update_args_v1_t,
    zx_clock_update_args_v2_t, zx_rights_t, zx_time_t,
};

use super::KernelHandle;
use super::clock_dispatcher_ffi::cpp_clock_dispatcher_create;
use super::dispatcher::DispatcherOps;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    ClockDispatcherState,
    object_constants::kClockDispatcherStateSize,
    object_constants::kClockDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_CLOCK_CREATE_COUNT, "dispatcher.clock.create", Sum);
define_kcounter!(DISPATCHER_CLOCK_DESTROY_COUNT, "dispatcher.clock.destroy", Sum);

/// Helper copying affine transform to zx_clock_transformation_t.
fn copy_transform(src: &Transform) -> zx_clock_transformation_t {
    zx_clock_transformation_t {
        reference_offset: src.a_offset(),
        synthetic_offset: src.b_offset(),
        rate: zx_types::zx_clock_rate_t {
            synthetic_ticks: src.numerator(),
            reference_ticks: src.denominator(),
        },
    }
}

/// RAII guard that disables interrupts and holds the SeqLock writer lock.
///
/// TODO(johngro): Find a better place for this, or figure out a better way to
/// use the lockdep guards along with libfasttime
struct SeqGuard<'a> {
    // Declared before `_irq_guard` so that the writer lock is released before
    // interrupts are re-enabled.
    write_guard: WriteGuard<'a>,
    _irq_guard: InterruptDisableGuard,
}

impl<'a> SeqGuard<'a> {
    #[inline(always)]
    fn new(lock: &'a SeqLock) -> Self {
        let irq_guard = InterruptDisableGuard::new();
        Self { write_guard: lock.acquire(), _irq_guard: irq_guard }
    }

    /// Returns the sequence lock's write guard, which the payload accessors
    /// require as proof that the exclusive portion of the write cycle is in
    /// progress. This stands in for the C++ `__TA_GUARDED(seq_lock_)`
    /// annotations on the payloads.
    #[inline(always)]
    fn write_guard(&self) -> &WriteGuard<'a> {
        &self.write_guard
    }
}

/// Helpers which normalize access to the two versions of the update args.
pub trait ClockUpdateArgs {
    /// True if this struct is version 1 (`zx_clock_update_args_v1_t`).
    const IS_V1: bool;
    /// Returns the rate adjustment in PPM.
    fn rate_adjust(&self) -> i32;
    /// Returns the error bound in nanoseconds.
    fn error_bound(&self) -> u64;
    /// Returns the synthetic value in nanoseconds.
    fn synthetic_value(&self) -> i64;

    /// Reference value is an invalid field in the v1 struct, so v1 clock update
    /// structures report `None` here.
    fn reference_value(&self) -> Option<i64>;
}

impl ClockUpdateArgs for zx_clock_update_args_v1_t {
    const IS_V1: bool = true;
    fn rate_adjust(&self) -> i32 {
        self.rate_adjust
    }
    fn error_bound(&self) -> u64 {
        self.error_bound
    }
    fn synthetic_value(&self) -> i64 {
        self.value
    }
    fn reference_value(&self) -> Option<i64> {
        // v1 clock update structures have no reference value field.
        None
    }
}

impl ClockUpdateArgs for zx_clock_update_args_v2_t {
    const IS_V1: bool = false;
    fn rate_adjust(&self) -> i32 {
        self.rate_adjust
    }
    fn error_bound(&self) -> u64 {
        self.error_bound
    }
    fn synthetic_value(&self) -> i64 {
        self.synthetic_value
    }
    fn reference_value(&self) -> Option<i64> {
        Some(self.reference_value)
    }
}

/// Parameters for `ClockTransformation`, matching `fasttime::ClockTransformation::Params`.
///
/// Make sure that our params are both a standard layout, and a unique
/// representation.
///
/// The standard layout requirement ensures that we don't have wonky stuff like
/// a vtable, or multiple inheritance going on (just a plain 'ole structure
/// please), which is what `#[repr(C)]` gives us here.  The unique
/// representation requirement makes sure that we don't have any non-explicit
/// padding, which is what the exact size assertion below checks (the explicit
/// `_padding` field accounts for the only slack in the structure).  All of the
/// fields and their initial values are defined here and in the `Default`
/// implementation below.
///
/// Both of these things are important when we plan to share this data
/// structure between the kernel and user-mode, which we do.
#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable)]
struct ClockTransformationParams {
    reference_to_synthetic: Transform,
    error_bound: u64,
    last_value_update_ticks: i64,
    last_rate_adjust_update_ticks: i64,
    last_error_bounds_update_ticks: i64,
    cur_ppm_adj: i32,
    _padding: u32,
}

impl Default for ClockTransformationParams {
    /// Mirrors the default member initializers of
    /// `fasttime::ClockTransformation::Params`.
    fn default() -> Self {
        Self {
            reference_to_synthetic: Transform::new(0, 0, Ratio::new(0, 1)),
            error_bound: ZX_CLOCK_UNKNOWN_ERROR,
            last_value_update_ticks: 0,
            last_rate_adjust_update_ticks: 0,
            last_error_bounds_update_ticks: 0,
            cur_ppm_adj: 0,
            _padding: 0,
        }
    }
}

zr::static_assert_size_and_align!(ClockTransformationParams, 64, 8);

/// In-memory transformation structure matching `fasttime::ClockTransformation<Adapter>`.
///
/// # Layout is shared with user mode
///
/// For a mappable clock, this structure lives in the single page VMO which is
/// mapped (read-only) into user mode, and this Rust structure is the only
/// producer of that memory.  The VDSO consumes it by reinterpret_casting the
/// mapping to a `fasttime::ClockTransformation<Adapter>`; see
/// `//zircon/kernel/lib/userabi/vdso/zx_clock_read_mapped.cc`,
/// `//zircon/kernel/lib/userabi/vdso/zx_clock_get_details_mapped.cc`, and
/// `//src/starnix/kernel/vdso/vdso_calculate_utc.cc`.  The layout below is
/// therefore frozen; the matching C++ side assertions (which pin the C++
/// structure to these same offsets) live in
/// `//zircon/kernel/object/clock_dispatcher_ffi.cc`.
///
/// Because readers in user mode may observe the payloads while this side is
/// writing them, all payload accesses go through [`SeqLockPayload`], which
/// performs the transfers using per-element atomic operations.  Plain (or
/// `volatile`) accesses would be a formal data race.
#[repr(C, align(8))]
struct ClockTransformation {
    // Constants determined at construction time.  These never change and do not
    // need to be protected with the sequence lock.
    options: u64,
    backstop_time: zx_time_t,

    // The transformation "payload" parameters, and the sequence lock which protects them.
    //
    // Note that the reference_ticks_to_synthetic transformation is kept separate from the
    // rest of the parameters.  While we need to observe all of the parameters
    // during a call to get_details, we only need to observe reference_ticks_to_synthetic
    // during read, and keeping the parameters separate makes this a bit easier.
    seq_lock: SeqLock,
    // Explicit stand-in for the padding the C++ compiler inserts between the
    // sequence lock and the first payload.
    _pad: u32,
    reference_ticks_to_synthetic: SeqLockPayload<Transform>,
    params: SeqLockPayload<ClockTransformationParams>,
}

zr::static_assert_size_and_align!(
    ClockTransformation,
    object_constants::kClockTransformationStorageSize,
    object_constants::kClockTransformationStorageAlign,
);

// The transformation must fit in (and be alignable within) the page we hand to
// user mode.  Mirrors the static_asserts which used to live on
// `ClockDispatcher::ClockTransformationType`.
zr::static_assert!(
    core::mem::size_of::<ClockTransformation>() as u64 <= ClockDispatcher::MAPPED_SIZE
);
zr::static_assert!(
    core::mem::align_of::<ClockTransformation>() as u64 <= ClockDispatcher::MAPPED_SIZE
);

// The offsets, sizes and alignments below are shared with user mode; see the
// comment on `ClockTransformation` above and the matching C++ assertions in
// //zircon/kernel/object/clock_dispatcher_ffi.cc.
zr::static_assert!(core::mem::offset_of!(ClockTransformation, options) == 0);
zr::static_assert!(core::mem::offset_of!(ClockTransformation, backstop_time) == 8);
zr::static_assert!(core::mem::offset_of!(ClockTransformation, seq_lock) == 16);
zr::static_assert!(core::mem::offset_of!(ClockTransformation, reference_ticks_to_synthetic) == 24);
zr::static_assert!(core::mem::offset_of!(ClockTransformation, params) == 48);
zr::static_assert!(core::mem::size_of::<ClockTransformation>() == 112);
zr::static_assert!(core::mem::align_of::<ClockTransformation>() == 8);

zr::static_assert!(core::mem::offset_of!(ClockTransformationParams, reference_to_synthetic) == 0);
zr::static_assert!(core::mem::offset_of!(ClockTransformationParams, error_bound) == 24);
zr::static_assert!(core::mem::offset_of!(ClockTransformationParams, last_value_update_ticks) == 32);
zr::static_assert!(
    core::mem::offset_of!(ClockTransformationParams, last_rate_adjust_update_ticks) == 40
);
zr::static_assert!(
    core::mem::offset_of!(ClockTransformationParams, last_error_bounds_update_ticks) == 48
);
zr::static_assert!(core::mem::offset_of!(ClockTransformationParams, cur_ppm_adj) == 56);
zr::static_assert!(core::mem::offset_of!(ClockTransformationParams, _padding) == 60);
zr::static_assert!(core::mem::size_of::<ClockTransformationParams>() == 64);
zr::static_assert!(core::mem::align_of::<ClockTransformationParams>() == 8);

impl ClockTransformation {
    /// Initializes a `ClockTransformation` in-place within the provided storage pointer.
    ///
    /// # Safety
    ///
    /// `storage` must point to valid, 8-byte aligned memory for `ClockTransformation`.
    unsafe fn init_in(
        storage: *mut MaybeUninit<Self>,
        options: u64,
        backstop_time: zx_time_t,
    ) -> NonNull<Self> {
        // Initialize the transformation structure in-place in the chosen storage.
        // The payloads start with their default values and are then updated through
        // a `SeqLock` write transaction below, which also ensures that a freshly
        // created clock reports a generation counter of one.
        //
        // SAFETY: caller guarantees storage is valid and properly aligned.
        unsafe {
            storage.write(MaybeUninit::new(Self {
                options,
                backstop_time,
                seq_lock: SeqLock::new(),
                _pad: 0,
                reference_ticks_to_synthetic: SeqLockPayload::new(Transform::new(
                    0,
                    0,
                    Ratio::new(0, 1),
                )),
                params: SeqLockPayload::new(ClockTransformationParams::default()),
            }));
        }

        let storage = storage.cast::<Self>();

        // SAFETY: the transformation was just initialized in `storage`, which the
        // caller guarantees is valid and properly aligned.
        let this = unsafe { &*storage };

        // Initialize the internal transformation structure.
        let mut local_params = ClockTransformationParams::default();
        let local_ticks_to_synthetic;

        // Compute the initial state
        if (options & ZX_CLOCK_OPT_AUTO_START) != 0 {
            debug_assert!(backstop_time <= ClockDispatcher::get_current_time(this.is_boot()));
            let ticks_to_time_ratio = timer_get_ticks_to_time_ratio();
            let now_ticks = this.get_current_ticks();

            local_params.last_value_update_ticks = now_ticks;
            local_params.last_rate_adjust_update_ticks = now_ticks;
            local_ticks_to_synthetic = Transform::new(0, 0, ticks_to_time_ratio);
            local_params.reference_to_synthetic = Transform::new(0, 0, Ratio::new(1, 1));
        } else {
            local_ticks_to_synthetic = Transform::new(0, backstop_time, Ratio::new(0, 1));
            local_params.reference_to_synthetic =
                Transform::new(0, backstop_time, Ratio::new(0, 1));
        }

        // Publish the state from within the SeqLock
        {
            let guard = SeqGuard::new(&this.seq_lock);
            this.reference_ticks_to_synthetic
                .update(guard.write_guard(), &local_ticks_to_synthetic);
            this.params.update(guard.write_guard(), &local_params);
        }

        // SAFETY: `storage` is guaranteed non-null by the caller.
        unsafe { NonNull::new_unchecked(storage) }
    }

    fn is_monotonic(&self) -> bool {
        (self.options & ZX_CLOCK_OPT_MONOTONIC) != 0
    }

    fn is_boot(&self) -> bool {
        (self.options & ZX_CLOCK_OPT_BOOT) != 0
    }

    fn is_continuous(&self) -> bool {
        (self.options & ZX_CLOCK_OPT_CONTINUOUS) != 0
    }

    fn is_mappable(&self) -> bool {
        (self.options & ZX_CLOCK_OPT_MAPPABLE) != 0
    }

    /// Returns whether the clock has been started.
    ///
    /// Taking the writer's guard stands in for the C++ `__TA_REQUIRES(seq_lock_)`
    /// annotation.
    fn is_started(&self, guard: &SeqGuard<'_>) -> bool {
        // Note, we require that we hold the seq_lock exclusively here.  This
        // should ensure that there are no other threads writing to this memory
        // location concurrent with our read, meaning there is no formal data race
        // here.
        //
        // SAFETY: `guard` is the write guard of this transformation's sequence
        // lock, so no other writer can exist in this address space, and mappable
        // clocks are only ever mapped read-only into user mode, so user mode
        // cannot be writing the payload either.
        let params = unsafe { &*self.params.unsynchronized_get(guard.write_guard()) };
        params.reference_to_synthetic.numerator() != 0
    }

    fn get_current_ticks(&self) -> i64 {
        if self.is_boot() { timer_current_boot_ticks().0 } else { timer_current_mono_ticks().0 }
    }

    /// Reads the current synthetic time from the clock transformation.
    fn read(&self) -> Result<zx_time_t, Status> {
        let mut ticks_to_synthetic = Transform::default();
        let now_ticks = self.seq_lock.read_transaction(|| {
            self.reference_ticks_to_synthetic.read(&mut ticks_to_synthetic);
            self.get_current_ticks()
        });

        Ok(ticks_to_synthetic.apply::<{ Saturate::Yes }>(now_ticks))
    }

    /// Retrieves the fine-grained details of the clock transformation.
    fn get_details(&self) -> Result<zx_clock_details_v1_t, Status> {
        // Note that this cannot use the closure form of the read transaction,
        // because the details report the sequence number which the transaction
        // itself observed.
        let mut ticks_to_synthetic = Transform::default();
        let mut params = ClockTransformationParams::default();
        let (generation_counter, now_ticks) = loop {
            let token = self.seq_lock.begin_read_transaction();

            self.reference_ticks_to_synthetic.read(&mut ticks_to_synthetic);
            self.params.read(&mut params);
            let generation_counter = token.seq_num();
            let now_ticks = self.get_current_ticks();

            if self.seq_lock.end_read_transaction(token) {
                break (generation_counter, now_ticks);
            }
        };

        debug_assert!((generation_counter & 1) == 0);
        Ok(zx_clock_details_v1_t {
            generation_counter: generation_counter >> 1,
            reference_ticks_to_synthetic: copy_transform(&ticks_to_synthetic),
            reference_to_synthetic: copy_transform(&params.reference_to_synthetic),
            error_bound: params.error_bound,
            query_ticks: now_ticks,
            last_value_update_ticks: params.last_value_update_ticks,
            last_rate_adjust_update_ticks: params.last_rate_adjust_update_ticks,
            last_error_bounds_update_ticks: params.last_error_bounds_update_ticks,

            // Options and backstop_time are constant over the life of the clock.  We
            // don't need to latch them during the generation counter spin.
            options: self.options,
            backstop_time: self.backstop_time,
            ..Default::default()
        })
    }

    /// Updates the transformation parameters.
    fn update<Args: ClockUpdateArgs>(
        &self,
        disp: &ClockDispatcher,
        options: u64,
        args: &Args,
    ) -> Result<(), Status> {
        let do_set = (options & ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID) != 0;
        let do_rate = (options & ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID) != 0;
        let reference_valid = (options & ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID) != 0;

        // Perform the v1/v2 parameter sanity checks that we can perform without being
        // in the writer lock.
        if Args::IS_V1 {
            // v1 clocks are not allowed to specify a reference value (the v1 struct
            // does not have a field for it)
            if reference_valid {
                return Err(Status::INVALID_ARGS);
            }
        } else {
            // A reference value may only be provided during a V2 update as part of
            // either a value set, or rate change operation (or both).
            if reference_valid && !do_set && !do_rate {
                return Err(Status::INVALID_ARGS);
            }
        }

        let clock_was_started;
        {
            // Disable interrupts and enter the sequence lock exclusively, ensuring that
            // only one update can take place at a time. We disable interrupts for this
            // because this operation should be very quick, and we may have observers
            // who are spinning attempting to read the clock. We cannot afford to become
            // preempted while we are performing an update operation.
            let guard = SeqGuard::new(&self.seq_lock);

            // If the clock has not yet been started, then we require the first update
            // to include a set operation.
            if !do_set && !self.is_started(&guard) {
                return Err(Status::BAD_STATE);
            }

            // Continue with the argument sanity checking. Set operations are not
            // allowed on continuous clocks after the very first one (which is what
            // starts the clock).
            if do_set && self.is_continuous() && self.is_started(&guard) {
                return Err(Status::INVALID_ARGS);
            }

            // Checks specific to non-V1 update arguments.
            if !Args::IS_V1 {
                // The following checks only apply if the clock is a monotonic clock which
                // has already been started.
                if self.is_started(&guard) && self.is_monotonic() {
                    // Set operations for non-V1 update arguments made to a monotonic clock
                    // must supply an explicit reference time.
                    if do_set && !reference_valid {
                        return Err(Status::INVALID_ARGS);
                    }

                    // non-v1 set operations on monotonic clocks may not be combined with rate
                    // change operations. Additionally, rate change operations may not specify
                    // an explicit reference time when being applied to monotonic clocks.
                    if self.is_monotonic() && (do_set || reference_valid) && do_rate {
                        return Err(Status::INVALID_ARGS);
                    }
                }
            }

            // Make local copies of the core state. Note that we do not use either
            // acquire semantics on the loads during the copy, nor an acquire thread
            // fence. We currently have exclusive write access, so no other threads may
            // be writing to these variable as we read them, meaning that no data races
            // should exist here.
            let mut local_params = ClockTransformationParams::default();
            let mut local_ticks_to_synthetic = Transform::default();
            self.params.read(&mut local_params);
            self.reference_ticks_to_synthetic.read(&mut local_ticks_to_synthetic);

            // Mark the time at which this update will take place.
            let now_ticks = self.get_current_ticks();

            // Don't bother updating the structures representing the transformation if:
            //
            // 1) We are not changing either the value or rate, or
            // 2a) This is a rate-only change (the value is not being set)
            // 2b) With no explicit reference time provided
            // 2c) Which specifies the same rate that we are already using
            let skip_update = !do_set
                && (!do_rate
                    || (!reference_valid && (args.rate_adjust() == local_params.cur_ppm_adj)));

            // Now compute the new transformations
            if !skip_update {
                // Figure out the reference times at which this change will take place at.
                let ticks_to_time_ratio = timer_get_ticks_to_time_ratio();
                let now_mono = ticks_to_time_ratio.scale::<{ Round::DOWN }>(now_ticks);
                let mut reference_ticks = now_ticks;
                let mut reference_mono = now_mono;
                // Note that `reference_value` is `None` for v1 update arguments,
                // which have no reference value field.
                if reference_valid && let Some(value) = args.reference_value() {
                    reference_mono = value;
                    reference_ticks =
                        ticks_to_time_ratio.inverse().scale::<{ Round::DOWN }>(reference_mono);
                }

                // Next, figure out the synthetic value this clock will have after the
                // change. If this is a set operation, it will be the explicit value
                // provided by the user, otherwise it will be the synthetic value computed
                // using the old transformation applied to the target reference time.
                //
                // In the case that we need to compute the target synthetic time from a
                // previous transformation, use the old mono->synthetic time
                // transformation if the user explicitly supplied a monotonic reference
                // time for the update operation. Otherwise, use the old ticks->synthetic
                // time transformation along with reference ticks value which we observed
                // after entering the writer lock.
                //
                // In the case of a user supplied monotonic reference time, this avoids
                // rounding error ensures that the old and the new transformations both
                // pass through exactly the same [user_ref, synth] point (important during
                // testing).
                let target_synthetic = if do_set {
                    args.synthetic_value()
                } else if reference_valid {
                    local_params.reference_to_synthetic.apply::<{ Saturate::Yes }>(reference_mono)
                } else {
                    local_ticks_to_synthetic.apply::<{ Saturate::Yes }>(reference_ticks)
                };

                // Compute the new rate ratios.
                let (new_m2s_ratio, new_t2s_ratio) = if do_rate {
                    let new_m2s = Ratio::new((1_000_000 + args.rate_adjust()) as u32, 1_000_000);
                    let new_t2s = Ratio::product(ticks_to_time_ratio, new_m2s, Exact::No);
                    (new_m2s, new_t2s)
                } else if self.is_started(&guard) {
                    (local_params.reference_to_synthetic.ratio(), local_ticks_to_synthetic.ratio())
                } else {
                    (Ratio::new(1, 1), ticks_to_time_ratio)
                };

                // Update the local copies of the structures.
                let old_t2s = local_ticks_to_synthetic;
                local_params.reference_to_synthetic =
                    Transform::new(reference_mono, target_synthetic, new_m2s_ratio);
                local_ticks_to_synthetic =
                    Transform::new(reference_ticks, target_synthetic, new_t2s_ratio);

                // Make certain that the new transformations follow all of the rules
                // before applying them. In specific, we need to make certain that:
                //
                // 1) Monotonic clocks do not move backwards.
                // 2) Backstop times are not violated.
                //
                let new_synthetic_now =
                    local_ticks_to_synthetic.apply::<{ Saturate::Yes }>(now_ticks);
                if self.is_monotonic()
                    && (new_synthetic_now < old_t2s.apply::<{ Saturate::Yes }>(now_ticks))
                {
                    return Err(Status::INVALID_ARGS);
                }

                if new_synthetic_now < self.backstop_time {
                    return Err(Status::INVALID_ARGS);
                }
            }

            // Everything checks out, we can proceed with the update.
            // Record whether or not this is the initial start of the clock.
            clock_was_started = !self.is_started(&guard);

            // If this was a set operation, record the new last update time.
            if do_set {
                local_params.last_value_update_ticks = now_ticks;
            }

            // If this was a rate adjustment operation, or the clock was just started,
            // record the new last update time as well as the new current ppm
            // adjustment.
            if do_rate || clock_was_started {
                local_params.last_rate_adjust_update_ticks = now_ticks;
                local_params.cur_ppm_adj = if do_rate { args.rate_adjust() } else { 0 };
            }

            // If this was an error bounds update operations, record the new last update
            // time as well as the new error bound.
            if (options & ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID) != 0 {
                local_params.last_error_bounds_update_ticks = now_ticks;
                local_params.error_bound = args.error_bound();
            }

            // We are finished, publish the results in the shared structures.
            self.reference_ticks_to_synthetic
                .update(guard.write_guard(), &local_ticks_to_synthetic);
            self.params.update(guard.write_guard(), &local_params);
        }

        // Now that we are out of the time critical section, if the clock was just
        // started, make sure to assert the ZX_CLOCK_STARTED signal to observers.
        let set_mask = if clock_was_started { ZX_CLOCK_STARTED } else { 0 };

        // Pulse ZX_CLOCK_UPDATED to announce that a clock update has occurred.
        disp.update_state_with_strobe(0, set_mask, ZX_CLOCK_UPDATED);

        Ok(())
    }
}

/// Internal state storage for `ClockDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ClockDispatcherState {
    canary: Canary<{ fbl::magic(b"CLOK") }>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    local_storage: zr::Opaque<MaybeUninit<ClockTransformation>>,
    vmo: Option<RefPtr<VmObject>>,
    transformation: NonNull<ClockTransformation>,

    #[pin]
    name: Name<ZX_MAX_NAME_LEN>,
}

impl ClockDispatcherState {
    /// Initializes the `ClockDispatcherState`.
    pub(crate) fn init(
        dispatcher: &ClockDispatcher,
        options: u64,
        backstop_time: zx_time_t,
        vmo: Option<RefPtr<VmObjectPaged>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        let mappable_storage = if let Some(vmo) = vmo.as_ref() {
            debug_assert!((options & ZX_CLOCK_OPT_MAPPABLE) != 0);

            // Find the physical address of our VMO's (single) page, then use it to
            // locate the kernel view of that page in the kernel's flat map.  There
            // should be no possible way for this to fail, so unconditionally assert
            // that everything goes as we expect.
            //
            // SAFETY: `vmo` was just allocated with `ALWAYS_PINNED` and a single page,
            // and `page_request` is `None`.
            let (_, pa) = match unsafe { vmo.get_page(0, 0, None) } {
                Ok(res) => res,
                Err(res) => {
                    panic!("Failed to get storage page for mappable clock ({})", res.into_raw())
                }
            };
            assert!(
                is_physmap_phys_addr(pa),
                "Mappable clock storage page is not in the physmap {:#018x}",
                pa.0
            );

            // Set the user-id of our VMO to be the same as our KOID.  This way, when a
            // mapped clock is enumerated in a diagnostic info call, the KOID of this
            // clock will be what gets reported in the info record.
            vmo.set_user_id(dispatcher.get_koid());

            // Clocks (as kernel objects) currently don't have names, so we cannot use a
            // similar trick to apply a name to how our mapped clock is reported.  For
            // now, just set the name of the underlying VMO to "kernel-clock", so that
            // it will be clear to someone looking at diagnostic info that the mapping
            // is for a clock.
            const DEFAULT_NAME: &[u8] = b"kernel-clock";
            let _ = vmo.set_name(DEFAULT_NAME);

            Some(core::ptr::with_exposed_provenance_mut::<MaybeUninit<ClockTransformation>>(
                paddr_to_physmap(pa).0,
            ))
        } else {
            debug_assert!((options & ZX_CLOCK_OPT_MAPPABLE) == 0);
            None
        };

        pin_init!(&this in Self {
            canary: Canary::new(),
            lock <- KMutex::init(),
            local_storage: zr::Opaque::uninit(),
            vmo: vmo.map(VmObjectPaged::into_vm_object),
            // Find our storage for our clock transformation, either in our VMO if we are
            // mappable, or in our local storage if not.
            //
            // Note that `transformation` must be initialized *after* `local_storage`;
            // for a non-mappable clock it reads `(*this).local_storage` and
            // constructs the transformation there.  Reordering these two fields
            // would leave the transformation pointing at storage which is about to
            // be overwritten.
            transformation: {
                let storage = mappable_storage
                    .unwrap_or_else(|| unsafe { (*this.as_ptr()).local_storage.get() });
                // Initialize our transformation structure in-place in our storage of choice.
                // SAFETY: `storage` is non-null and points to valid storage for ClockTransformation.
                unsafe { ClockTransformation::init_in(storage, options, backstop_time) }
            },
            name <- Name::init(),
            _: {
                // If we auto-started our clock, update our state.
                if (options & ZX_CLOCK_OPT_AUTO_START) != 0 {
                    dispatcher.update_state(0, ZX_CLOCK_STARTED);
                }

                DISPATCHER_CLOCK_CREATE_COUNT.add(1);
            },
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ClockDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        let this = self.project();
        // Explicitly destruct our clock transformation instance before its underlying
        // storage goes away.
        // SAFETY: `transformation` was initialized and is valid until dropped here.
        unsafe {
            core::ptr::drop_in_place(this.transformation.as_ptr());
        }
        DISPATCHER_CLOCK_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct ClockDispatcher,
    ClockDispatcherState,
    ZX_OBJ_TYPE_CLOCK,
    object_constants::kClockDispatcherStateOffset
);

impl ClockDispatcher {
    /// The size of the mapped region for mappable clocks.
    pub const MAPPED_SIZE: u64 = PAGE_SIZE as u64;

    /// Returns the default rights for a clock handle.
    const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_CLOCK_RIGHTS
    }

    fn get_current_time(boot_time: bool) -> zx_time_t {
        if boot_time { current_boot_time().0 } else { current_mono_time().0 }
    }

    /// Creates a new `ClockDispatcher` and returns its kernel handle and assigned rights.
    pub fn create(
        mut options: u64,
        create_args: &zx_clock_create_args_v1_t,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // The syscall_ layer has already parsed our args version and extracted them
        // into our |create_args| argument as appropriate.  Go ahead and discard the
        // version information before sanity checking the rest of the options.
        options &= !ZX_CLOCK_ARGS_VERSION_MASK;

        // Reject any request which includes an options flag we do not recognize.
        if (options & !ZX_CLOCK_OPTS_ALL) != 0 {
            return Err(Status::INVALID_ARGS);
        }

        // If the user asks for a continuous clock, it must also be monotonic.
        if (options & ZX_CLOCK_OPT_CONTINUOUS) != 0 && (options & ZX_CLOCK_OPT_MONOTONIC) == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // Make sure that the backstop time is valid. If this clock is being created
        // with the "auto start" flag, then it begins life as a clone of its reference
        // clock (either monotonic or boot), and the backstop time has to be <= the
        // current reference clock value. Otherwise, the clock starts in the stopped
        // state, and any specified backstop time must simply be non-negative.
        let now = Self::get_current_time((options & ZX_CLOCK_OPT_BOOT) != 0);
        if ((options & ZX_CLOCK_OPT_AUTO_START) != 0 && (create_args.backstop_time > now))
            || create_args.backstop_time < 0
        {
            return Err(Status::INVALID_ARGS);
        }

        // If the user requested a map-able clock, create a single-page VMO which we
        // will use to share clock state with our user.
        let vmo = if (options & ZX_CLOCK_OPT_MAPPABLE) != 0 {
            // Make sure to allocate our VMO with the `ALWAYS_PINNED` flag, for two
            // reasons.
            //
            // 1) To save a bit of time and overhead, we use the physmap view of this
            //    page in the kernel in order to access the actual memory.  If the page
            //    backing this clock is not pinned, then this technique is no good.  _In
            //    theory_, the page could be re-claimed then restored (to a different
            //    physical location) invalidating our kernel-physmap view of the memory
            //    in the process.  This cannot be allowed to happen.
            // 2) Even if we make a kernel-specific PTE for the kernel view of the
            //    memory (instead of using the physmap view), it needs to be accessed
            //    from inside of a spinlock-equivalent (the exclusive form of the
            //    seq-lock) during an Update operation.  We are going to be touching the
            //    memory, but cannot allow a page fault during this operation, so it is
            //    important that it always remain pinned.
            let vmo_paged = VmObjectPaged::create(
                ALLOC_FLAG_ANY | ALLOC_FLAG_CAN_WAIT,
                VmObjectPaged::ALWAYS_PINNED,
                Self::MAPPED_SIZE,
            )?;
            Some(vmo_paged)
        } else {
            None
        };

        let raw_vmo = vmo.map(|v| RefPtr::into_raw(v).cast_mut()).unwrap_or(core::ptr::null_mut());
        // SAFETY: `cpp_clock_dispatcher_create` initializes `out` on ZX_OK.
        let handle = unsafe {
            KernelHandle::create(|out| {
                cpp_clock_dispatcher_create(options, create_args.backstop_time, raw_vmo, out)
            })
        }?;

        // The new clock instance should have the default rights, plus the "map" right
        // if the clock was created as map-able.
        let rights = Self::default_rights()
            | if (options & ZX_CLOCK_OPT_MAPPABLE) != 0 { ZX_RIGHT_MAP } else { 0 };

        Ok((handle, rights))
    }

    /// Reads the current synthetic time from the clock.
    pub fn read(&self) -> Result<zx_time_t, Status> {
        self.state().canary.assert();
        // SAFETY: `transformation` is valid for the lifetime of `ClockDispatcherState`.
        unsafe { self.state().transformation.as_ref().read() }
    }

    /// Fetches the fine-grained details of the clock object.
    pub fn get_details(&self) -> Result<zx_clock_details_v1_t, Status> {
        self.state().canary.assert();
        // SAFETY: `transformation` is valid for the lifetime of `ClockDispatcherState`.
        unsafe { self.state().transformation.as_ref().get_details() }
    }

    /// Updates the clock using either version 1 or version 2 update parameters.
    pub fn update<Args: ClockUpdateArgs>(&self, options: u64, args: &Args) -> Result<(), Status> {
        self.state().canary.assert();
        // SAFETY: `transformation` is valid for the lifetime of `ClockDispatcherState`.
        unsafe { self.state().transformation.as_ref().update(self, options, args) }
    }

    /// Gets the name of the clock.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) -> Result<(), Status> {
        self.state().canary.assert();
        self.state().name.get(out_name);
        Ok(())
    }

    /// Sets the name of the clock.
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        self.state().canary.assert();
        crate::ktrace_rs::kernel_object!("kernel:meta", self.get_koid(), ZX_OBJ_TYPE_CLOCK, name);
        self.state().name.set(name);
        Ok(())
    }

    /// Returns the underlying VMO for a mappable clock, if any.
    pub fn vmo(&self) -> Option<&RefPtr<VmObject>> {
        self.state().canary.assert();
        self.state().vmo.as_ref()
    }

    /// Returns whether the clock is mappable into a VMAR.
    pub fn is_mappable(&self) -> bool {
        self.state().canary.assert();
        // SAFETY: `transformation` is valid for the lifetime of `ClockDispatcherState`.
        unsafe { self.state().transformation.as_ref().is_mappable() }
    }

    /// Returns the defined mapped size for mappable clocks.
    pub fn get_mapped_size(&self) -> Result<u64, Status> {
        // Only mappable clocks have a defined mapped size.
        if !self.is_mappable() {
            return Err(Status::INVALID_ARGS);
        }
        Ok(Self::MAPPED_SIZE)
    }
}

/// In-tree kernel unit tests for `ClockDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "clock_dispatcher_rust")]
mod tests {
    use super::{ClockDispatcher, ZX_DEFAULT_CLOCK_RIGHTS};
    use crate::platform_rs::timer::current_mono_time;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true};
    use zx_status::Status;
    use zx_types::{
        ZX_CLOCK_OPT_AUTO_START, ZX_CLOCK_OPT_CONTINUOUS, ZX_CLOCK_OPT_MAPPABLE,
        ZX_CLOCK_OPT_MONOTONIC, ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID,
        ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID, ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID,
        ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, ZX_MAX_NAME_LEN, ZX_RIGHT_MAP,
        zx_clock_create_args_v1_t, zx_clock_update_args_v1_t, zx_clock_update_args_v2_t, zx_time_t,
    };

    /// Tests creating a clock with default options.
    #[test]
    fn test_default_create() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (handle, rights) = ClockDispatcher::create(0, &args).expect("failed to create clock");
        expect_eq!(rights, ZX_DEFAULT_CLOCK_RIGHTS);

        let disp = handle.dispatcher();
        expect_true!(disp.get_koid() != 0);
        expect_false!(disp.is_mappable());
        expect_true!(disp.vmo().is_none());
        expect_true!(disp.get_mapped_size() == Err(Status::INVALID_ARGS));

        // An unstarted clock returns its backstop time when read.
        let time = disp.read().expect("failed to read unstarted clock");
        expect_eq!(time, 0);
        let details = disp.get_details().expect("failed to get clock details");
        expect_eq!(details.backstop_time, 0);
    }

    /// Tests creating an auto-started clock.
    #[test]
    fn test_auto_start_create() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (handle, rights) = ClockDispatcher::create(ZX_CLOCK_OPT_AUTO_START, &args)
            .expect("failed to create auto-start clock");
        expect_eq!(rights, ZX_DEFAULT_CLOCK_RIGHTS);

        let disp = handle.dispatcher();
        expect_false!(disp.is_mappable());

        // Auto-started clock is already started and readable.
        let time = disp.read().expect("failed to read auto-started clock");
        expect_true!(time >= 0);

        let details = disp.get_details().expect("failed to get clock details");
        expect_eq!(details.backstop_time, 0);
    }

    /// Tests creating a mappable clock.
    #[test]
    fn test_mappable_create() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (handle, rights) = ClockDispatcher::create(ZX_CLOCK_OPT_MAPPABLE, &args)
            .expect("failed to create mappable clock");
        expect_eq!(rights, ZX_DEFAULT_CLOCK_RIGHTS | ZX_RIGHT_MAP);

        let disp = handle.dispatcher();
        expect_true!(disp.is_mappable());
        expect_true!(disp.vmo().is_some());
        expect_eq!(disp.get_mapped_size().unwrap(), ClockDispatcher::MAPPED_SIZE);
        let vmo = disp.vmo().unwrap();
        expect_eq!(vmo.size(), ClockDispatcher::MAPPED_SIZE);
    }

    /// Tests parameter validation during clock creation.
    #[test]
    fn test_create_validation() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };

        // Invalid options: unknown option bit.
        expect_true!(ClockDispatcher::create(1 << 30, &args).err() == Some(Status::INVALID_ARGS));

        // Continuous clock without monotonic flag.
        expect_true!(
            ClockDispatcher::create(ZX_CLOCK_OPT_CONTINUOUS, &args).err()
                == Some(Status::INVALID_ARGS)
        );

        // Negative backstop time.
        let bad_backstop = zx_clock_create_args_v1_t { backstop_time: -1 };
        expect_true!(ClockDispatcher::create(0, &bad_backstop).err() == Some(Status::INVALID_ARGS));

        // Future backstop time with auto-start.
        let future_backstop = zx_clock_create_args_v1_t { backstop_time: zx_time_t::MAX };
        expect_true!(
            ClockDispatcher::create(ZX_CLOCK_OPT_AUTO_START, &future_backstop).err()
                == Some(Status::INVALID_ARGS)
        );
    }

    /// Tests setting and getting the clock name property.
    #[test]
    fn test_get_set_name() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (handle, _) = ClockDispatcher::create(0, &args).expect("failed to create clock");
        let disp = handle.dispatcher();

        let mut name = [0u8; ZX_MAX_NAME_LEN];
        expect_ok!(disp.get_name(&mut name));
        expect_true!(name == [0u8; ZX_MAX_NAME_LEN]);

        expect_ok!(disp.set_name(b"test-clock"));
        expect_ok!(disp.get_name(&mut name));
        let nul_pos = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        expect_true!(&name[..nul_pos] == b"test-clock");

        // Embedded null should truncate the name.
        expect_ok!(disp.set_name(b"truncated\0ignored"));
        expect_ok!(disp.get_name(&mut name));
        let nul_pos = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        expect_true!(&name[..nul_pos] == b"truncated");

        // Longer name should be truncated to ZX_MAX_NAME_LEN - 1.
        expect_ok!(disp.set_name(
            b"this-is-a-very-long-name-that-exceeds-the-maximum-allowed-length-for-a-clock-name"
        ));
        expect_ok!(disp.get_name(&mut name));
        let nul_pos = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        expect_eq!(nul_pos, ZX_MAX_NAME_LEN - 1);
        expect_true!(&name[..nul_pos] == b"this-is-a-very-long-name-that-e");
    }

    /// Tests starting and updating a clock via update with v1 args.
    #[test]
    fn test_update_v1() {
        let args = zx_clock_create_args_v1_t { backstop_time: 100 };
        let (handle, _) = ClockDispatcher::create(0, &args).expect("failed to create clock");
        let disp = handle.dispatcher();

        // Starting the clock with a set operation
        let mut update_args = zx_clock_update_args_v1_t::default();
        update_args.value = 200;
        update_args.error_bound = 10;
        let options =
            ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID | ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID;
        expect_ok!(disp.update(options, &update_args));

        let now = disp.read().expect("failed to read clock");
        expect_true!(now >= 200);

        let details = disp.get_details().expect("failed to get details");
        expect_eq!(details.error_bound, 10);
        expect_eq!(details.backstop_time, 100);
    }

    /// Tests updating a clock using `zx_clock_update_args_v2_t` with explicit `reference_value` and rate adjustment.
    #[test]
    fn test_update_v2() {
        let args = zx_clock_create_args_v1_t { backstop_time: 100 };
        let (handle, _) = ClockDispatcher::create(0, &args).expect("failed to create clock");
        let disp = handle.dispatcher();

        let now_mono = current_mono_time().0;
        let mut v2_args = zx_clock_update_args_v2_t::default();
        v2_args.synthetic_value = 1000;
        v2_args.reference_value = now_mono;
        v2_args.rate_adjust = 50;
        v2_args.error_bound = 25;
        let options = ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID
            | ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID
            | ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID
            | ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID;
        expect_ok!(disp.update(options, &v2_args));

        let details = disp.get_details().expect("failed to get details");
        expect_eq!(details.error_bound, 25);
        expect_eq!(details.reference_to_synthetic.reference_offset, now_mono);
        expect_eq!(details.reference_to_synthetic.synthetic_offset, 1000);
        expect_eq!(details.reference_to_synthetic.rate.synthetic_ticks, 1_000_050);
        expect_eq!(details.reference_to_synthetic.rate.reference_ticks, 1_000_000);
    }

    /// Tests that `details.generation_counter` is 1 initially and increments on each update.
    #[test]
    fn test_generation_counter() {
        let args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (handle, _) = ClockDispatcher::create(0, &args).expect("failed to create clock");
        let disp = handle.dispatcher();

        let details = disp.get_details().expect("failed to get details");
        expect_eq!(details.generation_counter, 1);

        let mut v1_args = zx_clock_update_args_v1_t::default();
        v1_args.value = 1000;
        expect_ok!(disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args));
        let details = disp.get_details().expect("failed to get details");
        expect_eq!(details.generation_counter, 2);

        v1_args.error_bound = 5;
        expect_ok!(disp.update(ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID, &v1_args));
        let details = disp.get_details().expect("failed to get details");
        expect_eq!(details.generation_counter, 3);
    }

    /// Tests error conditions during `update`: BAD_STATE and INVALID_ARGS for v1 and v2.
    #[test]
    fn test_update_errors() {
        let create_args = zx_clock_create_args_v1_t { backstop_time: 1000 };
        let (handle, _) = ClockDispatcher::create(0, &create_args).expect("failed to create clock");
        let disp = handle.dispatcher();

        let mut v1_args = zx_clock_update_args_v1_t::default();

        // 1. Updating an unstarted clock without ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID returns BAD_STATE.
        v1_args.error_bound = 10;
        expect_true!(
            disp.update(ZX_CLOCK_UPDATE_OPTION_ERROR_BOUND_VALID, &v1_args)
                == Err(Status::BAD_STATE)
        );

        // 2. Passing reference_valid on v1 returns INVALID_ARGS.
        v1_args.value = 2000;
        expect_true!(
            disp.update(
                ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID
                    | ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID,
                &v1_args
            ) == Err(Status::INVALID_ARGS)
        );

        // 3. Violating backstop_time returns INVALID_ARGS.
        v1_args.value = 500;
        expect_true!(
            disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args)
                == Err(Status::INVALID_ARGS)
        );

        // 4. Setting a continuous clock after start returns INVALID_ARGS.
        let cont_args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (cont_handle, _) =
            ClockDispatcher::create(ZX_CLOCK_OPT_MONOTONIC | ZX_CLOCK_OPT_CONTINUOUS, &cont_args)
                .expect("failed to create continuous clock");
        let cont_disp = cont_handle.dispatcher();
        v1_args.value = 500;
        expect_ok!(cont_disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args));
        v1_args.value = 600;
        expect_true!(
            cont_disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args)
                == Err(Status::INVALID_ARGS)
        );

        // 5. Setting a monotonic clock backwards returns INVALID_ARGS.
        let mono_args = zx_clock_create_args_v1_t { backstop_time: 0 };
        let (mono_handle, _) = ClockDispatcher::create(ZX_CLOCK_OPT_MONOTONIC, &mono_args)
            .expect("failed to create monotonic clock");
        let mono_disp = mono_handle.dispatcher();
        v1_args.value = 10_000_000_000;
        expect_ok!(mono_disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args));
        let current_synth = mono_disp.read().expect("failed to read monotonic clock");
        v1_args.value = current_synth - 5_000_000_000;
        expect_true!(
            mono_disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v1_args)
                == Err(Status::INVALID_ARGS)
        );

        // 6. V2 monotonic rules on started monotonic clock:
        let mut v2_args = zx_clock_update_args_v2_t::default();
        v2_args.synthetic_value = current_synth + 5_000_000_000;

        // 6a. V2 set operation on started monotonic clock without reference_valid returns INVALID_ARGS.
        expect_true!(
            mono_disp.update(ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID, &v2_args)
                == Err(Status::INVALID_ARGS)
        );

        // 6b. V2 set operation combined with rate adjust on started monotonic clock returns INVALID_ARGS.
        v2_args.reference_value = current_mono_time().0;
        v2_args.rate_adjust = 10;
        expect_true!(
            mono_disp.update(
                ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID
                    | ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID
                    | ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID,
                &v2_args
            ) == Err(Status::INVALID_ARGS)
        );

        // 6c. V2 rate adjust with explicit reference_valid on started monotonic clock returns INVALID_ARGS.
        expect_true!(
            mono_disp.update(
                ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID
                    | ZX_CLOCK_UPDATE_OPTION_RATE_ADJUST_VALID,
                &v2_args
            ) == Err(Status::INVALID_ARGS)
        );
    }
}
