// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Types and structures for kernel scheduler state (`include/kernel/scheduler_state.h`).

use crate::kernel::types::{cpu_mask_t, cpu_num_t};
use core::mem::{MaybeUninit, align_of, size_of};
use object_constants_rs as object_constants;
use wait_queue_bindings as bindings;
use zr::OpaqueFacade;

/// The scheduling state of a thread matching C++ `enum thread_state : uint8_t`.
// LINT.IfChange(ThreadStateKind)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadStateKind {
    /// Thread has been created but not yet made runnable.
    Initial = 0,
    /// Thread is ready to run and in a run queue.
    Ready = 1,
    /// Thread is currently running on a CPU.
    Running = 2,
    /// Thread is blocked waiting on an event, mutex, or condition.
    Blocked = 3,
    /// Thread is blocked on a read lock.
    BlockedReadLock = 4,
    /// Thread is sleeping.
    Sleeping = 5,
    /// Thread is suspended.
    Suspended = 6,
    /// Thread is dead and awaiting cleanup.
    Death = 7,
}

// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:thread_state)

zr::static_assert!(core::mem::size_of::<ThreadStateKind>() == 1);
zr::static_assert!(core::mem::align_of::<ThreadStateKind>() == 1);

/// Facade for the C++ `SchedulerState` class.
///
/// # Safety Invariant
///
/// Any `&SchedulerState` or `&mut SchedulerState` reference must point to a valid, initialized
/// C++ `SchedulerState` instance guarded by its owning [`Thread`](crate::kernel::thread::Thread)'s
/// lock.
#[repr(C)]
pub struct SchedulerState {
    // Safety invariant: `_facade` is zero-sized and private so safe code cannot construct a
    // `SchedulerState` by value; any reference points to a live, initialized `SchedulerState`
    // guarded by its owning `Thread`'s lock, and `OpaqueFacade` marks `SchedulerState` as
    // `!Freeze` and `!Unpin` for FFI mutation.
    _facade: OpaqueFacade,
}

zr::static_assert!(core::mem::size_of::<SchedulerState>() == 0);

impl SchedulerState {
    /// Returns the CPU the thread is currently assigned to, or `INVALID_CPU` if it is not
    /// currently assigned to a CPU.
    #[inline]
    pub fn curr_cpu(&self) -> cpu_num_t {
        // SAFETY: `self` is a valid `SchedulerState` reference.
        unsafe { cpp_scheduler_state_curr_cpu(self) }
    }

    /// Returns the CPU the thread last ran on, or `INVALID_CPU` if it has never run.
    #[inline]
    pub fn last_cpu(&self) -> cpu_num_t {
        // SAFETY: `self` is a valid `SchedulerState` reference.
        unsafe { cpp_scheduler_state_last_cpu(self) }
    }

    /// Returns the hard CPU affinity mask of the thread.
    #[inline]
    pub fn hard_affinity(&self) -> cpu_mask_t {
        // SAFETY: `self` is a valid `SchedulerState` reference.
        unsafe { cpp_scheduler_state_hard_affinity(self) }
    }

    /// Recomputes the effective profile of this scheduler state.
    #[inline]
    pub fn recompute_effective_profile(&mut self) {
        // SAFETY: `self` is a valid mutable `SchedulerState` reference (guarded by the owning
        // `Thread`'s lock), which has the same address and layout as
        // `crate::kernel::scheduler::SchedulerState`.
        let state = unsafe {
            &mut *core::ptr::from_mut(self).cast::<crate::kernel::scheduler::SchedulerState>()
        };
        state.recompute_effective_profile();
    }
}

/// Opaque wrapper around C++ `SchedulerState::BaseProfile`.
#[repr(C, align(8))]
pub struct SchedulerStateBaseProfile(
    zr::OpaqueBytes<{ object_constants::kSchedulerStateBaseProfileSize }>,
);

zr::static_assert_size_and_align!(
    SchedulerStateBaseProfile,
    object_constants::kSchedulerStateBaseProfileSize,
    object_constants::kSchedulerStateBaseProfileAlign,
);

unsafe extern "C" {
    fn cpp_scheduler_state_base_profile_init_fair(
        out_profile: *mut MaybeUninit<SchedulerStateBaseProfile>,
        priority: i32,
        inheritable: bool,
    );
    fn cpp_scheduler_state_base_profile_init_deadline(
        out_profile: *mut MaybeUninit<SchedulerStateBaseProfile>,
        capacity_ns: zx_types::zx_duration_mono_t,
        deadline_ns: zx_types::zx_duration_mono_t,
    );
    fn cpp_scheduler_state_curr_cpu(state: *const SchedulerState) -> cpu_num_t;
    fn cpp_scheduler_state_last_cpu(state: *const SchedulerState) -> cpu_num_t;
    fn cpp_scheduler_state_hard_affinity(state: *const SchedulerState) -> cpu_mask_t;
}

// thread priority
/// Number of priority levels.
pub const NUM_PRIORITIES: i32 = 32;
/// Lowest priority level.
pub const LOWEST_PRIORITY: i32 = 0;
/// Highest priority level.
pub const HIGHEST_PRIORITY: i32 = NUM_PRIORITIES - 1;
/// Priority level for DPC threads.
pub const DPC_PRIORITY: i32 = NUM_PRIORITIES - 2;
/// Priority level for idle threads.
pub const IDLE_PRIORITY: i32 = LOWEST_PRIORITY;
/// Low priority level.
pub const LOW_PRIORITY: i32 = NUM_PRIORITIES / 4;
/// Default thread priority level.
pub const DEFAULT_PRIORITY: i32 = NUM_PRIORITIES / 2;
/// High priority level.
pub const HIGH_PRIORITY: i32 = (NUM_PRIORITIES / 4) * 3;

impl SchedulerStateBaseProfile {
    /// Creates a fair `SchedulerStateBaseProfile` with the given priority and inheritable flag.
    ///
    /// # Panics
    ///
    /// Panics if `priority` is outside `LOWEST_PRIORITY..=HIGHEST_PRIORITY`.
    pub fn fair(priority: i32, inheritable: bool) -> Self {
        assert!((LOWEST_PRIORITY..=HIGHEST_PRIORITY).contains(&priority));
        let mut profile = MaybeUninit::uninit();
        // SAFETY: `priority` is validated to be within `LOWEST_PRIORITY..=HIGHEST_PRIORITY`, and
        // `cpp_scheduler_state_base_profile_init_fair` unconditionally initializes a scheduler
        // state into the provided storage.
        unsafe {
            cpp_scheduler_state_base_profile_init_fair(&mut profile, priority, inheritable);
            profile.assume_init()
        }
    }

    /// Creates a deadline `SchedulerStateBaseProfile` with the given capacity and deadline.
    ///
    /// # Panics
    ///
    /// Panics if `capacity.0 <= 0` or `capacity.0 > deadline.0`.
    pub fn deadline(
        capacity: crate::platform_rs::timer::DurationMono,
        deadline: crate::platform_rs::timer::DurationMono,
    ) -> Self {
        assert!(capacity.0 > 0 && capacity.0 <= deadline.0);
        let mut profile = MaybeUninit::uninit();
        // SAFETY: `cpp_scheduler_state_base_profile_init_deadline` unconditionally initializes
        // a deadline base profile into the provided storage, and `0 < capacity <= deadline` is
        // verified above.
        unsafe {
            cpp_scheduler_state_base_profile_init_deadline(&mut profile, capacity.0, deadline.0);
            profile.assume_init()
        }
    }

    /// Returns a raw pointer to the underlying byte storage.
    pub fn get(&self) -> *mut [u8; object_constants::kSchedulerStateBaseProfileSize] {
        self.0.get()
    }
}

/// An opaque type representing the C++ `SchedulerState::EffectiveProfile` struct.
#[repr(C)]
pub struct SchedulerStateEffectiveProfile {
    raw: zr::Opaque<bindings::SchedulerState_EffectiveProfile>,
}

zr::static_assert!(
    size_of::<SchedulerStateEffectiveProfile>()
        == size_of::<bindings::SchedulerState_EffectiveProfile>()
);
zr::static_assert!(
    align_of::<SchedulerStateEffectiveProfile>()
        == align_of::<bindings::SchedulerState_EffectiveProfile>()
);
