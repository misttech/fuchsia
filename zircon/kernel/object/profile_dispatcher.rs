// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use fbl::Canary;
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_PROFILE_RIGHTS, ZX_OBJ_TYPE_PROFILE, ZX_PRIORITY_DEFAULT, ZX_PRIORITY_HIGH,
    ZX_PROFILE_INFO_FLAG_CPU_MASK, ZX_PROFILE_INFO_FLAG_CRITICAL, ZX_PROFILE_INFO_FLAG_DEADLINE,
    ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY, ZX_PROFILE_INFO_FLAG_NO_INHERIT,
    ZX_PROFILE_INFO_FLAG_PRIORITY, zx_cpu_set_t, zx_profile_info_t, zx_rights_t,
};

use super::KernelHandle;
use super::profile_dispatcher_ffi::cpp_profile_dispatcher_create;
use super::thread_dispatcher::ThreadDispatcher;
use super::vm_address_region_dispatcher::{MemoryPriority, VmAddressRegionDispatcher};
use crate::counters;
use crate::kernel::scheduler::SchedCompactDuration;
use crate::kernel::scheduler_state::{
    HIGHEST_PRIORITY, LOWEST_PRIORITY, SchedulerStateBaseProfile,
};
use crate::kernel::types::cpu_mask_t;
use crate::platform_rs::timer::DurationMono;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    ProfileDispatcherState,
    object_constants::kProfileDispatcherStateSize,
    object_constants::kProfileDispatcherStateAlign,
);

counters::define_kcounter!(DISPATCHER_PROFILE_CREATE_COUNT, "dispatcher.profile.create", Sum);
counters::define_kcounter!(DISPATCHER_PROFILE_DESTROY_COUNT, "dispatcher.profile.destroy", Sum);

fn parse_cpu_mask(set: &zx_cpu_set_t) -> cpu_mask_t {
    // The code below only supports reading up to 1 word in the mask.
    zr::static_assert!(counters::SMP_MAX_CPUS <= core::mem::size_of::<u64>() * 8);
    zr::static_assert!(counters::SMP_MAX_CPUS <= core::mem::size_of::<cpu_mask_t>() * 8);
    zr::static_assert!(counters::SMP_MAX_CPUS <= zx_types::ZX_CPU_SET_MAX_CPUS);

    // We throw away any bits beyond SMP_MAX_CPUs.
    (set.mask[0] as cpu_mask_t) & crate::kernel::bits::bit_mask_u32(counters::SMP_MAX_CPUS)
}

fn validate_and_create_profile(
    info: &zx_profile_info_t,
) -> Result<SchedulerStateBaseProfile, Status> {
    // Ensure that none of the flags outside of the set of valid flags has been set.
    const ALL_FLAGS: u32 = ZX_PROFILE_INFO_FLAG_PRIORITY
        | ZX_PROFILE_INFO_FLAG_CPU_MASK
        | ZX_PROFILE_INFO_FLAG_DEADLINE
        | ZX_PROFILE_INFO_FLAG_NO_INHERIT
        | ZX_PROFILE_INFO_FLAG_CRITICAL;
    if (info.flags & !ALL_FLAGS) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // Ensure that exactly one of the "discipline selection" flags has been set.
    const DISCIPLINE_SELECTION_FLAGS: u32 =
        ZX_PROFILE_INFO_FLAG_PRIORITY | ZX_PROFILE_INFO_FLAG_DEADLINE;
    if (info.flags & DISCIPLINE_SELECTION_FLAGS).count_ones() != 1 {
        return Err(Status::INVALID_ARGS);
    }

    // Deadline profiles may not be flagged as NO_INHERIT.
    const DEADLINE_NO_INHERIT: u32 =
        ZX_PROFILE_INFO_FLAG_DEADLINE | ZX_PROFILE_INFO_FLAG_NO_INHERIT;
    if (info.flags & DEADLINE_NO_INHERIT) == DEADLINE_NO_INHERIT {
        return Err(Status::INVALID_ARGS);
    }

    let inheritable = (info.flags & ZX_PROFILE_INFO_FLAG_NO_INHERIT) == 0;

    if (info.flags & ZX_PROFILE_INFO_FLAG_PRIORITY) != 0 {
        if (info.flags & ZX_PROFILE_INFO_FLAG_CRITICAL) != 0 {
            return Err(Status::INVALID_ARGS);
        }
        // SAFETY: `zx_profile_info_t` is initialized from `FromBytes` (or zeroed), and
        // `ZX_PROFILE_INFO_FLAG_PRIORITY` selects `priority_params`.
        let priority = unsafe { info.zx_profile_info_union.priority_params.priority };
        if !(LOWEST_PRIORITY..=HIGHEST_PRIORITY).contains(&priority) {
            return Err(Status::INVALID_ARGS);
        }
        Ok(SchedulerStateBaseProfile::fair(priority, inheritable))
    } else {
        debug_assert!(inheritable);
        // SAFETY: `zx_profile_info_t` is initialized from `FromBytes` (or zeroed), and
        // `ZX_PROFILE_INFO_FLAG_DEADLINE` selects `deadline_params`.
        let params = unsafe { info.zx_profile_info_union.deadline_params };
        let admissible = params.capacity > 0
            && params.capacity <= params.relative_deadline
            && params.relative_deadline <= params.period
            && inheritable;
        if !admissible {
            return Err(Status::INVALID_ARGS);
        }

        // Check that the parameters are within the range of the signed 32-bit
        // `SchedCompactDuration`. This permits more compact storage of profile
        // parameters and provides protection against excessive values.
        let max_compact = i64::from(SchedCompactDuration::max().raw_value());
        if params.capacity > max_compact
            || params.relative_deadline > max_compact
            || params.period > max_compact
        {
            return Err(Status::OUT_OF_RANGE);
        }

        let critical = (info.flags & ZX_PROFILE_INFO_FLAG_CRITICAL) != 0;
        Ok(SchedulerStateBaseProfile::deadline_with_critical(
            DurationMono::from_nanos(params.capacity),
            DurationMono::from_nanos(params.relative_deadline),
            critical,
        ))
    }
}

fn parse_memory_priority(info: &zx_profile_info_t) -> Result<MemoryPriority, Status> {
    // SAFETY: info.flags has ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY set.
    let priority = unsafe { info.zx_profile_info_union.priority_params.priority };
    if priority == ZX_PRIORITY_HIGH {
        Ok(MemoryPriority::High)
    } else if priority == ZX_PRIORITY_DEFAULT {
        Ok(MemoryPriority::Default)
    } else {
        Err(Status::INVALID_ARGS)
    }
}

const SCHED_FLAGS: u32 = ZX_PROFILE_INFO_FLAG_PRIORITY | ZX_PROFILE_INFO_FLAG_DEADLINE;
const AFFINITY_FLAGS: u32 = ZX_PROFILE_INFO_FLAG_CPU_MASK;
const THREAD_FLAGS: u32 = SCHED_FLAGS | AFFINITY_FLAGS;
const MEMORY_FLAGS: u32 = ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY;
const REQUIRED_FLAGS: u32 = THREAD_FLAGS | MEMORY_FLAGS;

struct ParsedProfileInfo {
    profile: Option<SchedulerStateBaseProfile>,
    cpu_mask: Option<cpu_mask_t>,
    memory_priority: Option<MemoryPriority>,
}

fn parse_profile_info(info: &zx_profile_info_t) -> Result<ParsedProfileInfo, Status> {
    if (info.flags & REQUIRED_FLAGS) == 0 {
        return Err(Status::INVALID_ARGS);
    }

    if (info.flags & MEMORY_FLAGS) != 0 && (info.flags & THREAD_FLAGS) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let profile = if (info.flags & SCHED_FLAGS) != 0 {
        Some(validate_and_create_profile(info)?)
    } else {
        None
    };

    let cpu_mask = if (info.flags & AFFINITY_FLAGS) != 0 {
        Some(parse_cpu_mask(&info.cpu_affinity_mask))
    } else {
        None
    };

    let memory_priority =
        if (info.flags & MEMORY_FLAGS) != 0 { Some(parse_memory_priority(info)?) } else { None };

    Ok(ParsedProfileInfo { profile, cpu_mask, memory_priority })
}

#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ProfileDispatcherState {
    canary: Canary<{ fbl::magic(b"PROF") }>,

    profile: Option<SchedulerStateBaseProfile>,
    cpu_mask: Option<cpu_mask_t>,
    memory_priority: Option<MemoryPriority>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl ProfileDispatcherState {
    /// Initializes a `ProfileDispatcherState` from a pointer to `Option<ParsedProfileInfo>`.
    ///
    /// # Safety
    ///
    /// `parsed_info` must point to a valid `Some(ParsedProfileInfo)`.
    pub(super) unsafe fn init(
        _dispatcher: *const ProfileDispatcher,
        parsed_info: *mut c_void,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: `parsed_info` points to `Some(ParsedProfileInfo)` on
        // `ProfileDispatcher::create`'s stack and is consumed at most once.
        let ParsedProfileInfo { profile, cpu_mask, memory_priority } =
            unsafe { (*parsed_info.cast::<Option<ParsedProfileInfo>>()).take() }
                .expect("pre-parsed profile info");

        DISPATCHER_PROFILE_CREATE_COUNT.add(1);

        pin_init!(Self {
            canary: Canary::new(),
            profile,
            cpu_mask,
            memory_priority,
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ProfileDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_PROFILE_DESTROY_COUNT.add(1);
    }
}

super::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct ProfileDispatcher,
    ProfileDispatcherState,
    ZX_OBJ_TYPE_PROFILE,
    object_constants::kProfileDispatcherStateOffset
);

impl ProfileDispatcher {
    pub fn create(info: &zx_profile_info_t) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let mut parsed = Some(parse_profile_info(info)?);
        let parsed_ptr = (&raw mut parsed).cast::<c_void>();
        // SAFETY: `cpp_profile_dispatcher_create` forwards `parsed_ptr` to
        // `ProfileDispatcherState::init` and initializes `handle` on success.
        let handle =
            unsafe { KernelHandle::create(|out| cpp_profile_dispatcher_create(parsed_ptr, out)) }?;
        Ok((handle, ZX_DEFAULT_PROFILE_RIGHTS))
    }

    pub fn apply_profile_to_thread(&self, thread: &ThreadDispatcher) -> Result<(), Status> {
        let state = self.state();
        if let Some(ref profile) = state.profile {
            thread.set_base_profile(profile)?;
        }

        if let Some(mask) = state.cpu_mask {
            thread.set_soft_affinity(mask)?;
        }

        Ok(())
    }

    pub fn apply_profile_to_vmar(&self, vmar: &VmAddressRegionDispatcher) -> Result<(), Status> {
        let state = self.state();
        if let Some(memory_priority) = state.memory_priority {
            vmar.set_memory_priority(memory_priority)?;
        }
        Ok(())
    }
}
