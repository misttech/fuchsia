// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::thread::{Thread, ThreadPtr};
use crate::platform_rs::timer::InstantMono;
use crate::vm::compression::VmCompression;
use crate::vm::page::VmPagePtr;
use crate::vm::vm_cow_pages::{
    EvictionAction, VmCowReclaimFailure, VmCowReclaimSuccess, VmCowReclaimType,
};
use crate::vm::{page_queues, pmm};
use core::fmt::Write;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use evictor_bindings as bindings;
use fbl::StringBuffer;
use ksync::{KMutex, RawMonitoredSpinlock, RawMutex, guarded, source_tag};
use pin_init::{PinInit, pin_data, pin_init, stack_pin_init};
use zr::SendSyncInlineFn;

define_kcounter!(
    PAGER_BACKED_PAGES_EVICTED,
    "vm.reclamation.pages_evicted_pager_backed.total",
    Sum
);
define_kcounter!(
    PAGER_BACKED_PAGES_EVICTED_OOM,
    "vm.reclamation.pages_evicted_pager_backed.oom",
    Sum
);
define_kcounter!(COMPRESSION_EVICTED, "vm.reclamation.pages_evicted_compressed.total", Sum);
define_kcounter!(COMPRESSION_EVICTED_OOM, "vm.reclamation.pages_evicted_compressed.oom", Sum);
define_kcounter!(DISCARDABLE_PAGES_EVICTED, "vm.reclamation.pages_evicted_discardable.total", Sum);
define_kcounter!(
    DISCARDABLE_PAGES_EVICTED_OOM,
    "vm.reclamation.pages_evicted_discardable.oom",
    Sum
);

#[inline]
fn checked_increment(a: &mut u64, b: u64) {
    let (result, overflow) = a.overflowing_add(b);
    debug_assert!(!overflow);
    *a = result;
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvictionLevel {
    OnlyOldest = 0,
    IncludeNewest = 1,
}

// Compile-time layout assertions against C++ EvictionLevel
zr::static_assert!(
    core::mem::size_of::<EvictionLevel>()
        == core::mem::size_of::<bindings::Evictor_EvictionLevel>()
);
zr::static_assert!(
    EvictionLevel::OnlyOldest as u8 == bindings::Evictor_EvictionLevel::OnlyOldest as u8
);
zr::static_assert!(
    EvictionLevel::IncludeNewest as u8 == bindings::Evictor_EvictionLevel::IncludeNewest as u8
);

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum Output {
    NoPrint = 0,
    Print = 1,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum TriggerReason {
    Other = 0,
    OOM = 1,
}

/// Eviction target state is grouped together behind a lock to allow different threads to safely
/// trigger and perform the eviction.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct EvictionTarget {
    pub pending: bool,
    /// The desired value to get the PMM node's free page count to.
    pub free_pages_target: u64,
    /// A minimum amount of pages we want to evict, regardless of how much free memory is
    /// available.
    pub min_pages_to_free: u64,
    pub level: EvictionLevel,
    pub print_counts: bool,
    pub oom_trigger: bool,
}

impl Default for EvictionTarget {
    fn default() -> Self {
        Self {
            pending: false,
            free_pages_target: 0,
            min_pages_to_free: 0,
            level: EvictionLevel::OnlyOldest,
            print_counts: false,
            oom_trigger: false,
        }
    }
}

// Compile-time layout assertions against C++ EvictionTarget
zr::static_assert!(
    core::mem::size_of::<EvictionTarget>()
        == core::mem::size_of::<bindings::Evictor_EvictionTarget>()
);
zr::static_assert!(
    core::mem::align_of::<EvictionTarget>()
        == core::mem::align_of::<bindings::Evictor_EvictionTarget>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, pending)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, pending)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, free_pages_target)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, free_pages_target)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, min_pages_to_free)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, min_pages_to_free)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, level)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, level)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, print_counts)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, print_counts)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionTarget, oom_trigger)
        == core::mem::offset_of!(bindings::Evictor_EvictionTarget, oom_trigger)
);

/// We count non-loaned and loaned evicted pages separately since the eviction goal is set in terms
/// of non-loaned pages (for now) so in order to verify expected behavior in tests we keep separate
/// counts.
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct EvictedPageCounts {
    /// The pager_backed and pager_backed_loaned counts are exclusive; any given evicted page is
    /// counted as pager_backed or pager_backed_loaned depending on whether it's non-loaned or
    /// loaned respectively.
    ///
    /// evicted from pager-backed VMO non-loaned page count
    pub pager_backed: u64,
    /// evicted from pager-backed VMO loaned page count
    pub pager_backed_loaned: u64,
    /// evicted from/via discardable VMO page count
    pub discardable: u64,
    /// evicted from an anonymous VMO via compression
    pub compressed: u64,
}

impl EvictedPageCounts {
    pub const fn non_loaned_total(&self) -> u64 {
        self.pager_backed + self.discardable + self.compressed
    }
}

impl core::ops::AddAssign for EvictedPageCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.pager_backed += rhs.pager_backed;
        self.pager_backed_loaned += rhs.pager_backed_loaned;
        self.discardable += rhs.discardable;
        self.compressed += rhs.compressed;
    }
}

impl core::ops::Sub for EvictedPageCounts {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            pager_backed: self.pager_backed - rhs.pager_backed,
            pager_backed_loaned: self.pager_backed_loaned - rhs.pager_backed_loaned,
            discardable: self.discardable - rhs.discardable,
            compressed: self.compressed - rhs.compressed,
        }
    }
}

// Compile-time layout assertions against C++ EvictedPageCounts
zr::static_assert!(
    core::mem::size_of::<EvictedPageCounts>()
        == core::mem::size_of::<bindings::Evictor_EvictedPageCounts>()
);
zr::static_assert!(
    core::mem::align_of::<EvictedPageCounts>()
        == core::mem::align_of::<bindings::Evictor_EvictedPageCounts>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, pager_backed)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, pager_backed)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, pager_backed_loaned)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, pager_backed_loaned)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, discardable)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, discardable)
);
zr::static_assert!(
    core::mem::offset_of!(EvictedPageCounts, compressed)
        == core::mem::offset_of!(bindings::Evictor_EvictedPageCounts, compressed)
);

/// Result returned from eviction methods indicating what they performed and why they returned.
/// Provides by the count of how many different items were evicted, which could be more or less
/// than requested, as well as whether any free memory target was achieved.
#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EvictionResult {
    pub counts: EvictedPageCounts,
    pub free_target_reached: bool,
}

// Compile-time layout assertions against C++ EvictionResult
zr::static_assert!(
    core::mem::size_of::<EvictionResult>()
        == core::mem::size_of::<bindings::Evictor_EvictionResult>()
);
zr::static_assert!(
    core::mem::align_of::<EvictionResult>()
        == core::mem::align_of::<bindings::Evictor_EvictionResult>()
);
zr::static_assert!(
    core::mem::offset_of!(EvictionResult, counts)
        == core::mem::offset_of!(bindings::Evictor_EvictionResult, counts)
);
zr::static_assert!(
    core::mem::offset_of!(EvictionResult, free_target_reached)
        == core::mem::offset_of!(bindings::Evictor_EvictionResult, free_target_reached)
);

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct EvictorStats {
    pub pager_backed_oom: u64,
    pub pager_backed_other: u64,
    pub compression_oom: u64,
    pub compression_other: u64,
    pub discarded_oom: u64,
    pub discarded_other: u64,
}

pub type ReclaimFunction = SendSyncInlineFn<
    'static,
    fn(
        Option<&'static VmCompression>,
        EvictionLevel,
    ) -> Option<Result<VmCowReclaimSuccess, VmCowReclaimFailure>>,
>;

pub type FreePagesFunction = SendSyncInlineFn<'static, fn() -> u64>;

/// Implements page evictor logic to free pages belonging to a PmmNode under memory pressure.
/// Eviction in this context is both direct eviction of pager backed memory and discardable VMO
/// memory, as well as by performing compression.
/// This class is thread-safe.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Evictor {
    /// Target for eviction.
    #[guarded_by(lock)]
    eviction_target: EvictionTarget,

    /// Mutex that enforces only one eviction attempt to be active at any time. This prevents us
    /// from overshooting the free memory targets required by various simultaneous eviction
    /// requests.
    #[mutex]
    eviction_lock: KMutex<RawMutex>,

    /// Tracks the total amount of evictions performed by `evict_until_targets_met`. This allows
    /// parallel evictions to combine their eviction progress instead of acting serially.
    #[guarded_by(eviction_lock)]
    total_evicted: EvictedPageCounts,

    /// Use MonitoredSpinLock to provide lockup detector diagnostics for the critical sections
    /// protected by this lock.
    #[mutex]
    lock: KMutex<RawMonitoredSpinlock>,

    /// The eviction thread used to process asynchronous requests.
    /// Created only if eviction is enabled, i.e., `eviction_enabled` is set to true.
    ///
    /// Safety invariant: Either null or points to a live kernel `Thread` spawned by
    /// `enable_eviction` that has not yet been joined by `disable_eviction`. Only modified by
    /// `enable_eviction` and `disable_eviction`, which callers must not invoke concurrently.
    eviction_thread: AtomicPtr<Thread>,
    eviction_thread_exiting: AtomicBool,

    /// Used by the eviction thread to wait for eviction requests.
    #[pin]
    eviction_signal: AutounsignalEvent,

    /// Optionally specified methods to allow for tests to fake interactions with the pmm. To avoid
    /// virtual dispatch in non test scenarios these are empty/null methods when not set.
    test_reclaim_function: Option<ReclaimFunction>,
    test_free_pages_function: Option<FreePagesFunction>,

    // These parameters are initialized later from kernel cmdline options.
    /// Whether eviction is enabled.
    #[guarded_by(lock)]
    eviction_enabled: bool,
    /// Whether eviction should attempt compression.
    #[guarded_by(lock)]
    use_compression: bool,

    #[pin]
    _pin: PhantomPinned,
}

// Compile-time layout assertions against the C++ Evictor type and StateLayout via bindgen.
zr::static_assert!(core::mem::size_of::<Evictor>() == core::mem::size_of::<bindings::Evictor>());
zr::static_assert!(core::mem::align_of::<Evictor>() == core::mem::align_of::<bindings::Evictor>());
zr::static_assert!(
    core::mem::size_of::<Evictor>() == core::mem::size_of::<bindings::Evictor_StateLayout>()
);
zr::static_assert!(
    core::mem::align_of::<Evictor>() == core::mem::align_of::<bindings::Evictor_StateLayout>()
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_target)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_target)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_lock)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_lock)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, total_evicted)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, total_evicted)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, lock)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, lock)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_thread)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_thread)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_thread_exiting)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_thread_exiting)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_signal)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_signal)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, test_reclaim_function)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, test_reclaim_function)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, test_free_pages_function)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, test_free_pages_function)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, eviction_enabled)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, eviction_enabled)
);
zr::static_assert!(
    core::mem::offset_of!(Evictor, use_compression)
        == core::mem::offset_of!(bindings::Evictor_StateLayout, use_compression)
);

// SAFETY: `Evictor` is internally synchronized via `lock`, `eviction_lock`, and atomics, and its
// test callbacks (`SendSyncInlineFn`) are `Send + Sync`.
unsafe impl Send for Evictor {}
// SAFETY: `Evictor` methods operate on shared references `&self` concurrently across threads,
// with all mutable state protected by internal locks or atomics.
unsafe impl Sync for Evictor {}

#[pin_init::pinned_drop]
impl PinnedDrop for Evictor {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `self` is being dropped so no concurrent calls to `enable_eviction`,
        // `disable_eviction`, or `debug_get_evictor_thread` can occur, neither `lock` nor
        // `eviction_lock` is held by the current thread, and `drop` is not called from the
        // eviction thread itself.
        unsafe {
            self.disable_eviction();
        }
    }
}

pub type EvictorTestReclaimFn = unsafe extern "C" fn(
    ctx: *mut core::ffi::c_void,
    compression: *mut VmCompression,
    eviction_level: EvictionLevel,
    out_is_ok: *mut bool,
    out_success: *mut VmCowReclaimSuccess,
    out_failure: *mut VmCowReclaimFailure,
) -> bool;

pub type EvictorTestFreePagesFn = unsafe extern "C" fn(ctx: *mut core::ffi::c_void) -> u64;

fn reclaim_from_global_page_queues(
    compression: Option<&VmCompression>,
    eviction_level: EvictionLevel,
) -> Option<(Result<VmCowReclaimSuccess, VmCowReclaimFailure>, VmPagePtr)> {
    // Avoid evicting from the newest queue to prevent thrashing.
    let lowest_evict_queue = if eviction_level == EvictionLevel::IncludeNewest {
        page_queues::NUM_ACTIVE_QUEUES
    } else {
        page_queues::NUM_RECLAIM - page_queues::NUM_OLDEST_QUEUES
    };
    // If we're going to include newest pages, ignore eviction hints as well, i.e. also consider
    // evicting pages with always_need set if we encounter them in LRU order.
    let hint_action = if eviction_level == EvictionLevel::IncludeNewest {
        EvictionAction::IgnoreHint
    } else {
        EvictionAction::FollowHint
    };

    if let Some(backlink) = pmm::page_queues().peek_isolate(lowest_evict_queue) {
        // A valid backlink always has a valid cow
        debug_assert!(backlink.cow.is_some());
        let cow = backlink.cow.as_ref().unwrap();
        let res = if let Some(compression) = compression {
            stack_pin_init!(let compressor_guard = compression.acquire_compressor());
            let compressor = compressor_guard.get();
            if compressor.arm().is_err() {
                return None;
            }
            // SAFETY: `backlink.page` and `backlink.offset` were returned by `peek_isolate` for
            // `cow`, and `compressor` was just armed above.
            unsafe {
                cow.reclaim_page(backlink.page, backlink.offset, hint_action, Some(compressor))
            }
        } else {
            // SAFETY: `backlink.page` and `backlink.offset` were returned by `peek_isolate` for
            // `cow`.
            unsafe { cow.reclaim_page(backlink.page, backlink.offset, hint_action, None) }
        };
        return Some((res, backlink.page));
    }
    None
}

#[derive(Default)]
struct FailureReasons {
    compress_failed: u64,
    compress_accessed: u64,
    evict_accessed: u64,
    incorrect_page: u64,
    other: u64,
}

struct ReclaimFailureStats {
    consecutive_reclaim_failures: u64,
    failures_to_compare: u64,
    prev_page_evictions: u64,
    prev_evicted_page: Option<VmPagePtr>,
    // VmCowReclaimResult doesn't have a default constructor. Initialize to an arbitrary value;
    // this will be overwritten anyway by the actual result of eviction when one happens.
    prev_eviction_result: Result<VmCowReclaimSuccess, VmCowReclaimFailure>,
    printed_same_page_log: bool,
    failure_reasons: FailureReasons,
}

impl ReclaimFailureStats {
    const COMPARISON_BASE: u64 = 1000;

    fn new() -> Self {
        Self {
            consecutive_reclaim_failures: 0,
            failures_to_compare: Self::COMPARISON_BASE,
            prev_page_evictions: 0,
            prev_evicted_page: None,
            prev_eviction_result: Ok(VmCowReclaimSuccess {
                type_: VmCowReclaimType::Evict,
                num_pages: 0,
                num_loaned_pages: 0,
            }),
            printed_same_page_log: false,
            failure_reasons: FailureReasons::default(),
        }
    }

    fn update(&mut self, reclaim_failed: bool) {
        if reclaim_failed {
            self.consecutive_reclaim_failures += 1;
        } else {
            self.consecutive_reclaim_failures = 0;
            self.failures_to_compare = Self::COMPARISON_BASE;
            self.failure_reasons = FailureReasons::default();
        }
    }

    fn should_print_livelock(&mut self) -> bool {
        if self.consecutive_reclaim_failures == self.failures_to_compare {
            // Wait for longer before logging the next time to avoid spamming in an actual
            // livelock. Multiply checking for overflow.
            if let Some(next) = self.failures_to_compare.checked_mul(10) {
                self.failures_to_compare = next;
            }
            return true;
        }
        false
    }

    fn to_result_string(result: Result<VmCowReclaimSuccess, VmCowReclaimFailure>) -> &'static str {
        match result {
            Err(failure) => match failure {
                VmCowReclaimFailure::CompressFailed => "fail:compress",
                VmCowReclaimFailure::CompressAccessed => "fail:access_c",
                VmCowReclaimFailure::EvictAccessed => "fail:access_e",
                VmCowReclaimFailure::IncorrectPage => "fail:incorrect",
                VmCowReclaimFailure::Other => "fail:other",
            },
            Ok(success) => match success.type_ {
                VmCowReclaimType::Compress => "ok:compress",
                VmCowReclaimType::Discard => "ok:discard",
                VmCowReclaimType::Evict => "ok:evict",
            },
        }
    }

    fn check_for_same_page(
        &mut self,
        reclaimed_result: Result<VmCowReclaimSuccess, VmCowReclaimFailure>,
        evicted_page: Option<VmPagePtr>,
    ) {
        let prev_evicted_page = self.prev_evicted_page;
        let prev_eviction_result = self.prev_eviction_result;
        // Wait until the very end to update prev_evicted_page and prev_eviction_result, because we
        // will use the old values to perform the check.
        let _update_prev = zr::defer(|| {
            self.prev_evicted_page = evicted_page;
            self.prev_eviction_result = reclaimed_result;
        });

        if evicted_page != prev_evicted_page {
            return;
        }

        // Evicting the same page twice in a row indicates a potential bug in reclamation, unless
        // we're in the IncorrectPage failure case, since that indicates a race in page
        // ownership, and we can't expect the VmCowPages to have moved a page it does not
        // own out of the way. Every other failure case should have moved the page out of
        // the way (by calling mark_accessed), so we should not see the same page in the next
        // eviction attempt.
        //
        // It is also possible to see the same page in rare hard-to-check-for cases after having
        // been reclaimed and reused. See the discussion in https://fxbug.dev/434361683.
        if prev_eviction_result.is_err()
            && prev_eviction_result.unwrap_err() == VmCowReclaimFailure::IncorrectPage
        {
            return;
        }

        self.prev_page_evictions += 1;
        // Print only once per eviction attempt to prevent log spam.
        if !self.printed_same_page_log {
            self.printed_same_page_log = true;
            kprint::kprintln!(
                "Evictor reclaiming the same page again {:p} [prev {:s} cur {:s}]",
                evicted_page.map_or(core::ptr::null_mut(), |p| p.as_raw()),
                Self::to_result_string(prev_eviction_result),
                Self::to_result_string(reclaimed_result)
            );
        }
    }
}

fn diagnose_reclamation_failure(
    reclaimed_result: Result<VmCowReclaimSuccess, VmCowReclaimFailure>,
    evicted_page: Option<VmPagePtr>,
    failure_stats: &mut ReclaimFailureStats,
    has_test_reclaim: bool,
) {
    let mut reclaim_failed = false;
    if let Err(failure) = reclaimed_result {
        reclaim_failed = true;
        match failure {
            VmCowReclaimFailure::CompressFailed => {
                failure_stats.failure_reasons.compress_failed += 1;
            }
            VmCowReclaimFailure::CompressAccessed => {
                failure_stats.failure_reasons.compress_accessed += 1;
            }
            VmCowReclaimFailure::EvictAccessed => {
                failure_stats.failure_reasons.evict_accessed += 1;
            }
            VmCowReclaimFailure::IncorrectPage => {
                failure_stats.failure_reasons.incorrect_page += 1;
            }
            VmCowReclaimFailure::Other => {
                failure_stats.failure_reasons.other += 1;
            }
        }
    }

    // The test reclaim path might not even evict an actual page, so there's nothing to check.
    if !has_test_reclaim {
        failure_stats.check_for_same_page(reclaimed_result, evicted_page);
    }

    // If we've looped many times without being able to reclaim anything, make some noise.
    failure_stats.update(reclaim_failed);
    if failure_stats.should_print_livelock() {
        kprint::kprintln!(
            "Evictor failed {:u} reclaims in a row ({:u} for prev page), possible \
             livelock\ncompress_failed {:u} compress_accessed {:u} evict_accessed {:u} \
             incorrect_page {:u} other {:u}",
            failure_stats.consecutive_reclaim_failures,
            failure_stats.prev_page_evictions,
            failure_stats.failure_reasons.compress_failed,
            failure_stats.failure_reasons.compress_accessed,
            failure_stats.failure_reasons.evict_accessed,
            failure_stats.failure_reasons.incorrect_page,
            failure_stats.failure_reasons.other
        );
        pmm::page_queues().dump();
    }
}

impl Evictor {
    /// Private constructor for test code to specify custom methods to fake the reclamation.
    fn init_with_fns(
        test_reclaim_function: Option<ReclaimFunction>,
        test_free_pages_function: Option<FreePagesFunction>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            eviction_target: EvictionTarget::default().into(),
            eviction_lock <- KMutex::init(),
            total_evicted: EvictedPageCounts::default().into(),
            lock <- KMutex::init(),
            eviction_thread: AtomicPtr::new(core::ptr::null_mut()),
            eviction_thread_exiting: AtomicBool::new(false),
            eviction_signal <- AutounsignalEvent::init_unsignaled(),
            test_reclaim_function,
            test_free_pages_function,
            eviction_enabled: false.into(),
            use_compression: false.into(),
            _pin: PhantomPinned,
        })
    }

    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_with_fns(None, None)
    }

    /// Private constructor for test code to specify custom methods to fake the reclamation.
    pub fn init_for_test(
        reclaim_fn: ReclaimFunction,
        free_pages_fn: FreePagesFunction,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_with_fns(Some(reclaim_fn), Some(free_pages_fn))
    }

    /// Called from the scanner to enable eviction if required. Creates an eviction thread to
    /// process asynchronous eviction requests.
    /// By default this only enables user pager based eviction and `use_compression` can be used to
    /// also perform compression.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` remains pinned and valid until `disable_eviction` or `drop` completes.
    /// - This method is not called concurrently with `enable_eviction`, `disable_eviction`, or
    ///   `debug_get_evictor_thread`.
    /// - The current thread does not hold `self.lock`.
    pub unsafe fn enable_eviction(&self, use_compression: bool) {
        {
            ksync::lock!(let mut guard = self.lock_lock(source_tag!()));
            // It's an error to call this whilst the eviction thread is still exiting.
            assert!(!self.eviction_thread_exiting.load(Ordering::SeqCst));
            let fields = guard.as_mut().fields_mut();
            *fields.eviction_enabled = true;
            *fields.use_compression = use_compression;

            if !self.eviction_thread.load(Ordering::Relaxed).is_null() {
                return;
            }
        }

        // Set up the eviction thread to process asynchronous eviction requests.
        // SAFETY: `Self::eviction_thread_entry` is a valid thread entrypoint expecting `self` as
        // its argument; the caller guarantees `self` remains pinned and valid until the thread is
        // joined in `disable_eviction`, and `thread` was just created and has not been resumed or
        // joined yet.
        unsafe {
            let thread = crate::kernel::thread::create_with_priority(
                c"eviction-thread".as_ptr(),
                Self::eviction_thread_entry,
                (self as *const Self).cast_mut().cast(),
                crate::kernel::thread::DEFAULT_PRIORITY,
            );
            debug_assert!(thread.is_ok());
            let thread = thread.unwrap();
            self.eviction_thread.store(thread.as_raw(), Ordering::Relaxed);
            thread.resume();
        }
    }

    /// Called from the scanner to disable all eviction if needed, will shut down any in existing
    /// eviction thread. It is a responsibility of the scanner to not have multiple concurrent
    /// calls to this and `enable_eviction`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - This method is not called concurrently with `enable_eviction`, `disable_eviction`, or
    ///   `debug_get_evictor_thread`.
    /// - The current thread does not hold `self.lock` or `self.eviction_lock`, and is not the
    ///   eviction thread itself.
    pub unsafe fn disable_eviction(&self) {
        let eviction_thread: ThreadPtr;
        {
            // Grab the lock and update any state. We cannot actually wait for the eviction thread
            // to complete whilst the lock is held, however.
            ksync::lock!(let _guard = self.lock_lock(source_tag!()));
            // It's an error to call this in parallel with another `disable_eviction` call.
            assert!(!self.eviction_thread_exiting.load(Ordering::SeqCst));
            // SAFETY: By the `eviction_thread` field invariant and the caller's guarantee of no
            // concurrent `enable_eviction` or `disable_eviction` calls, a non-null pointer
            // references a live kernel `Thread`.
            let Some(thread) =
                (unsafe { ThreadPtr::from_raw(self.eviction_thread.load(Ordering::Relaxed)) })
            else {
                return;
            };
            eviction_thread = thread;
            self.eviction_thread_exiting.store(true, Ordering::SeqCst);
            self.eviction_signal.signal();
        }
        // Now with the lock dropped wait for the thread to complete. Use a locally cached copy of
        // the pointer so that even if the scanner performs a concurrent `enable_eviction` call
        // we should not crash or have races, although the eviction thread may fail to join.
        // SAFETY: `eviction_thread` is a valid live kernel thread that has been signaled to exit
        // and has not been joined yet.
        let res = unsafe { eviction_thread.join(InstantMono::INFINITE) };
        debug_assert_eq!(res, Ok(0));
        {
            ksync::lock!(let mut guard = self.lock_lock(source_tag!()));
            // Now update the state to indicate that eviction is disabled.
            self.eviction_thread.store(core::ptr::null_mut(), Ordering::Relaxed);
            *guard.as_mut().eviction_enabled_mut() = false;
            self.eviction_thread_exiting.store(false, Ordering::SeqCst);
        }
    }

    /// Evict from a user specified external `target` which is only used for this eviction attempt
    /// and does not interfere with the `eviction_target`.
    pub fn evict_from_external_target(&self, target: EvictionTarget) -> EvictionResult {
        self.evict_from_target_internal(target)
    }

    /// Performs a synchronous request to evict until free memory equals `free_mem_target` (in
    /// bytes) and at least `min_mem_to_free` (in bytes) has been reclaimed. The return value is an
    /// `EvictionResult` detailing exactly what was evicted and whether the `free_mem_target` was
    /// achieved. The `eviction_level` is a rough control that maps to how old a page needs to be
    /// for being considered for eviction. This may acquire arbitrary vmo and aspace locks.
    pub fn evict_synchronous(
        &self,
        min_mem_to_free: u64,
        free_mem_target: u64,
        eviction_level: EvictionLevel,
        output: Output,
        reason: TriggerReason,
    ) -> EvictionResult {
        if !self.is_eviction_enabled() {
            return EvictionResult::default();
        }
        let page_size = page::SIZE as u64;
        let target = EvictionTarget {
            pending: true,
            free_pages_target: free_mem_target / page_size,
            min_pages_to_free: min_mem_to_free / page_size,
            level: eviction_level,
            print_counts: output == Output::Print,
            oom_trigger: reason == TriggerReason::OOM,
        };

        self.evict_from_external_target(target)
    }

    /// Reclaim memory until free memory equals the `free_mem_target` (in bytes) and at least
    /// `min_mem_to_free` (in bytes) has been reclaimed. Reclamation will happen asynchronously on
    /// the eviction thread and this function returns immediately. Once the target is reached,
    /// or there is no more memory that can be reclaimed, this process will stop and the free
    /// memory target will be cleared. The `eviction_level` is a rough control on how hard to
    /// try and evict. Multiple calls to `evict_asynchronous` will cause all the targets to get
    /// merged by adding together `min_mem_to_free`, taking the max of `free_mem_target` and the
    /// highest or most aggressive of any `eviction_level`.
    pub fn evict_asynchronous(
        &self,
        min_mem_to_free: u64,
        free_mem_target: u64,
        eviction_level: EvictionLevel,
        output: Output,
    ) {
        if !self.is_eviction_enabled() {
            return;
        }
        let page_size = page::SIZE as u64;
        self.combine_eviction_target(EvictionTarget {
            pending: true,
            free_pages_target: free_mem_target / page_size,
            min_pages_to_free: min_mem_to_free / page_size,
            level: eviction_level,
            print_counts: output == Output::Print,
            oom_trigger: false,
        });
        // Unblock the eviction thread.
        self.eviction_signal.signal();
    }

    /// Whether any eviction can occur.
    pub fn is_eviction_enabled(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock(source_tag!()));
        *guard.eviction_enabled()
    }

    /// Whether eviction should attempt to use compression.
    pub fn is_compression_enabled(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock(source_tag!()));
        *guard.use_compression()
    }

    /// Return global eviction stats from all instantiations of the Evictor.
    pub fn get_global_stats() -> EvictorStats {
        let mut stats = EvictorStats::default();
        stats.pager_backed_oom = PAGER_BACKED_PAGES_EVICTED_OOM.sum_across_all_cpus() as u64;
        stats.pager_backed_other = (PAGER_BACKED_PAGES_EVICTED.sum_across_all_cpus() as u64)
            .wrapping_sub(stats.pager_backed_oom);
        stats.compression_oom = COMPRESSION_EVICTED_OOM.sum_across_all_cpus() as u64;
        stats.compression_other =
            (COMPRESSION_EVICTED.sum_across_all_cpus() as u64).wrapping_sub(stats.compression_oom);
        stats.discarded_oom = DISCARDABLE_PAGES_EVICTED_OOM.sum_across_all_cpus() as u64;
        stats.discarded_other = (DISCARDABLE_PAGES_EVICTED.sum_across_all_cpus() as u64)
            .wrapping_sub(stats.discarded_oom);
        stats
    }

    /// Debug method to retrieve any current eviction thread. Only to be used for testing /
    /// debugging purposes. It is up to the caller to know if this object is alive or not.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this method is not called concurrently with `enable_eviction`
    /// or `disable_eviction`, and that any returned `ThreadPtr` is not used after
    /// `disable_eviction` or `drop` is called.
    pub unsafe fn debug_get_evictor_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: By the `eviction_thread` field invariant and the caller's guarantee of no
        // concurrent `enable_eviction` or `disable_eviction` calls, if non-null the pointer
        // references a live kernel `Thread`.
        unsafe { ThreadPtr::from_raw(self.eviction_thread.load(Ordering::Relaxed)) }
    }

    /// Helpers for testing.
    pub fn debug_get_eviction_target(&self) -> EvictionTarget {
        ksync::lock!(let guard = self.lock_lock(source_tag!()));
        *guard.eviction_target()
    }

    /// Combine the specified `target` with the pre-existing `eviction_target`.
    pub fn combine_eviction_target(&self, target: EvictionTarget) {
        ksync::lock!(let mut guard = self.lock_lock(source_tag!()));
        let current = guard.as_mut().eviction_target_mut();
        current.pending = current.pending || target.pending;
        current.level = current.level.max(target.level);
        checked_increment(&mut current.min_pages_to_free, target.min_pages_to_free);
        current.free_pages_target = current.free_pages_target.max(target.free_pages_target);
        current.print_counts = current.print_counts || target.print_counts;
    }

    /// Perform eviction based on the current values of `eviction_target`. The expectation is that
    /// the user will have set the target before calling this function with
    /// `combine_eviction_target()`. This may acquire arbitrary vmo and aspace locks.
    pub fn evict_from_preloaded_target(&self) -> EvictedPageCounts {
        // Create a local copy of the eviction target to operate against.
        let target = {
            ksync::lock!(let guard = self.lock_lock(source_tag!()));
            *guard.eviction_target()
        };
        let result = self.evict_from_target_internal(target);
        {
            ksync::lock!(let mut guard = self.lock_lock(source_tag!()));
            let total = result.counts.non_loaned_total();
            // Clear the eviction target but retain any min pages that we might still need to free
            // in a subsequent eviction attempt.
            let current = guard.as_mut().eviction_target_mut();
            *current = EvictionTarget::default();
            current.min_pages_to_free = target.min_pages_to_free.saturating_sub(total);
        }
        result.counts
    }

    // Helper for evict_from_preloaded_target and evict_from_external_target.
    fn evict_from_target_internal(&self, target: EvictionTarget) -> EvictionResult {
        if !target.pending {
            return EvictionResult::default();
        }

        let free_pages_before = self.count_free_pages();
        let result = self.evict_until_targets_met(
            target.min_pages_to_free,
            target.free_pages_target,
            target.level,
            target.oom_trigger,
        );
        let free_pages_after = self.count_free_pages();

        if target.print_counts {
            const KB: u64 = 1024;
            const MB: u64 = 1024 * 1024;
            let page_size = page::SIZE as u64;
            // Helper lambdas to print the counts as values in KB or MB.
            let format_val = |count: u64| {
                let bytes = count * page_size;
                if bytes < MB { bytes / KB } else { bytes / MB }
            };
            let format_unit = |count: u64| if count < MB / page_size { "K" } else { "M" };

            // This should be large enough to hold the formatted string.
            const BUF_SIZE: usize = 56;
            let mut buf = StringBuffer::<BUF_SIZE>::new();
            if result.counts.pager_backed > 0 {
                let _ = write!(
                    buf,
                    " pager:{}{}",
                    format_val(result.counts.pager_backed),
                    format_unit(result.counts.pager_backed)
                );
            }
            if result.counts.discardable > 0 {
                assert!(buf.len() < BUF_SIZE);
                let _ = write!(
                    buf,
                    " discardable:{}{}",
                    format_val(result.counts.discardable),
                    format_unit(result.counts.discardable)
                );
            }
            if result.counts.compressed > 0 {
                assert!(buf.len() < BUF_SIZE);
                let _ = write!(
                    buf,
                    " compressed:{}{}",
                    format_val(result.counts.compressed),
                    format_unit(result.counts.compressed)
                );
            }
            if !buf.is_empty() {
                kprint::kprintln!(
                    "[EVICT]:{:s} free:{:u}M->{:u}M",
                    &*buf,
                    free_pages_before * page_size / MB,
                    free_pages_after * page_size / MB
                );
            }
        }

        if target.oom_trigger {
            PAGER_BACKED_PAGES_EVICTED_OOM.add(result.counts.pager_backed as i64);
            COMPRESSION_EVICTED_OOM.add(result.counts.compressed as i64);
            DISCARDABLE_PAGES_EVICTED_OOM.add(result.counts.discardable as i64);
        }

        result
    }

    // Evict until `min_pages_to_evict` have been evicted and there are at least
    // `free_pages_target` free pages on the system. Note that the eviction operation here is
    // one-shot, i.e. as soon as the targets are met, eviction will stop and the function will
    // return. Returns the number of discardable and pager-backed pages evicted and pages
    // compressed during the execution of this method. These counts include all parallel
    // invocations of this method and could be higher than requested if there was another eviction
    // happening. This may acquire arbitrary vmo and aspace locks.
    fn evict_until_targets_met(
        &self,
        min_pages_to_evict: u64,
        free_pages_target: u64,
        level: EvictionLevel,
        oom_trigger: bool,
    ) -> EvictionResult {
        if !self.is_eviction_enabled() {
            return EvictionResult::default();
        }

        // Helper to read the total_evicted counts under the lock.
        let read_counts = || {
            ksync::lock!(let guard = self.lock_eviction_lock());
            *guard.total_evicted()
        };

        // Take a snapshot of the eviction counts at the beginning of our attempts.
        let starting_counts = read_counts();
        let starting_non_loaned = starting_counts.non_loaned_total();

        let mut free_target_reached = false;
        loop {
            // Lock the eviction attempts so that we don't overshoot the free pages target.
            ksync::lock!(let mut evict_guard = self.lock_eviction_lock());

            let pages_freed_so_far =
                evict_guard.total_evicted().non_loaned_total() - starting_non_loaned;

            let free_pages = self.count_free_pages();
            let mut pages_to_free = 0;
            // Need to evict at least min_pages_to_evict, and then potentially up to the
            // free_pages_target.
            if pages_freed_so_far < min_pages_to_evict {
                pages_to_free = min_pages_to_evict - pages_freed_so_far;
            }
            if free_pages < free_pages_target {
                pages_to_free = (free_pages_target - free_pages).max(pages_to_free);
            } else {
                free_target_reached = true;
            }
            if pages_to_free == 0 {
                // The targets have been met. No more eviction is required right now.
                break;
            }

            // Cap the number of pages to free so that we do not monopolize the eviction_lock,
            // ensuring that parallel attempts at achieving different eviction outcomes
            // do not unnecessarily block each other.
            //
            // For OOM evictions we do not cap the number of pages so that we can evict as fast as
            // possible to resolve the OOM condition quickly.
            if !oom_trigger {
                pages_to_free = pages_to_free.min(128);
            }

            let pages_freed = self.evict_page_queues(pages_to_free, level);
            // Should we fail to free any pages then we give up and consider the eviction request
            // complete.
            if pages_freed.non_loaned_total() == 0 {
                break;
            }
            *evict_guard.as_mut().total_evicted_mut() += pages_freed;
        }

        // Return the difference of the current eviction counts compared to when we started.
        EvictionResult { counts: read_counts() - starting_counts, free_target_reached }
    }

    // Evict the requested number of `target_pages` from vmos by querying the page queues. The
    // returned struct has the number of pages evicted. The `eviction_level` is a rough control
    // that maps to how old a page needs to be for being considered for eviction. This may acquire
    // arbitrary vmo and aspace locks.
    fn evict_page_queues(
        &self,
        target_pages: u64,
        eviction_level: EvictionLevel,
    ) -> EvictedPageCounts {
        let mut counts = EvictedPageCounts::default();

        if !self.is_eviction_enabled() {
            return counts;
        }

        let compression =
            if self.is_compression_enabled() { pmm::node().get_page_compression() } else { None };

        let mut failure_stats = ReclaimFailureStats::new();
        let has_test_reclaim = self.test_reclaim_function.is_some();

        // Evict until we've counted enough pages to hit the target_pages. Explicitly do not
        // consider pager_backed_loaned towards our total, as loaned pages do not go to the
        // free memory pool.
        while counts.non_loaned_total() < target_pages {
            // Use the helper to perform a single 'step' of eviction.
            let Some((reclaimed_result, evicted_page)) =
                self.evict_page_queues_helper(compression, eviction_level)
            else {
                // An empty return from the helper indicates that there are no more eviction
                // candidates, so regardless of our desired target we must give up.
                break;
            };

            // Check for any reclamation failures and print diagnostic logs if needed.
            diagnose_reclamation_failure(
                reclaimed_result,
                evicted_page,
                &mut failure_stats,
                has_test_reclaim,
            );

            if let Ok(success) = reclaimed_result {
                let num_pages = success.num_pages;
                let num_loaned_pages = success.num_loaned_pages;

                match success.type_ {
                    VmCowReclaimType::Evict => {
                        counts.pager_backed += num_pages;
                        counts.pager_backed_loaned += num_loaned_pages;
                    }
                    VmCowReclaimType::Discard => {
                        debug_assert_eq!(num_loaned_pages, 0);
                        counts.discardable += num_pages;
                    }
                    VmCowReclaimType::Compress => {
                        debug_assert_eq!(num_loaned_pages, 0);
                        counts.compressed += num_pages;
                    }
                }
            }
        }

        PAGER_BACKED_PAGES_EVICTED.add((counts.pager_backed + counts.pager_backed_loaned) as i64);
        COMPRESSION_EVICTED.add(counts.compressed as i64);
        counts
    }

    extern "C" fn eviction_thread_entry(arg: *mut core::ffi::c_void) -> i32 {
        // SAFETY: `arg` was passed from `enable_eviction` as a valid pointer to `Self`, which
        // remains pinned and valid until the thread exits and is joined in `disable_eviction`.
        let evictor = unsafe { &*(arg as *const Self) };
        evictor.eviction_thread_loop()
    }

    // The main loop for the eviction thread.
    fn eviction_thread_loop(&self) -> i32 {
        while !self.eviction_thread_exiting.load(Ordering::SeqCst) {
            let _ = self.eviction_signal.wait_infinite();

            if self.eviction_thread_exiting.load(Ordering::SeqCst) {
                break;
            }

            // Process an eviction target if there is one. This is a no-op and no pages are evicted
            // if no target is pending.
            self.evict_from_preloaded_target();
        }
        0
    }

    // Returns the count of the free pages for use in performing target calculations. This could be
    // from the PMM or a test fake.
    fn count_free_pages(&self) -> u64 {
        if let Some(free_pages_fn) = &self.test_free_pages_function {
            return free_pages_fn.call();
        }
        pmm::node().count_free_pages()
    }

    // Helper method that performs a single 'step' of reclamation via the page queues. A return
    // value of some (even if the counts themselves are all zero) indicates that there is more
    // to reclamation that can be attempted, where as a None indicates there is nothing left to
    // try and reclaim.
    // This method will either reclaim from the global page queues, or receive fake data from a
    // test instance.
    // The page that reclamation was attempted on is also returned for debug purposes. This is only
    // populated for a real reclamation, not a fake one from a test instance.
    fn evict_page_queues_helper(
        &self,
        compression: Option<&'static VmCompression>,
        eviction_level: EvictionLevel,
    ) -> Option<(Result<VmCowReclaimSuccess, VmCowReclaimFailure>, Option<VmPagePtr>)> {
        if let Some(test_reclaim_function) = &self.test_reclaim_function {
            let test_ret_val = test_reclaim_function.call(compression, eviction_level);
            if let Some(val) = test_ret_val {
                return Some((val, None));
            }
            return None;
        }
        reclaim_from_global_page_queues(compression, eviction_level)
            .map(|(res, page)| (res, Some(page)))
    }
}

#[derive(Copy, Clone)]
struct TestCallbackCtx(*mut core::ffi::c_void);

// SAFETY: `TestCallbackCtx` is only constructed in `rust_evictor_init_with_test_functions`, whose
// caller guarantees that `ctx` may be passed to `reclaim_fn` and `free_pages_fn` from any thread
// for the lifetime of the `Evictor`.
unsafe impl Send for TestCallbackCtx {}
// SAFETY: `TestCallbackCtx` is only constructed in `rust_evictor_init_with_test_functions`, whose
// caller guarantees that `ctx` may be passed to `reclaim_fn` and `free_pages_fn` concurrently
// from any thread for the lifetime of the `Evictor`.
unsafe impl Sync for TestCallbackCtx {}

impl TestCallbackCtx {
    fn as_ptr(self) -> *mut core::ffi::c_void {
        self.0
    }
}

// C FFI exports for C++ `Evictor` methods

/// Initializes an `Evictor` in place at `evictor`.
///
/// # Safety
///
/// The caller must ensure that `evictor` is a valid, aligned, non-null pointer to uninitialized
/// storage suitable for `Evictor` that will remain pinned until destroyed by
/// `rust_evictor_destroy`.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_init(evictor: *mut Evictor) {
    // SAFETY: The caller guarantees `evictor` points to valid, aligned, uninitialized storage
    // that will remain pinned.
    let _ = unsafe { Evictor::init().__pinned_init(evictor) };
}

/// Initializes an `Evictor` in place at `evictor` with custom test functions.
///
/// # Safety
///
/// The caller must ensure that:
/// - `evictor` is a valid, aligned, non-null pointer to uninitialized storage suitable for
///   `Evictor` that will remain pinned until destroyed by `rust_evictor_destroy`.
/// - `ctx`, `reclaim_fn`, and `free_pages_fn` remain valid and thread-safe for concurrent calls
///   across threads for the lifetime of `evictor`, and do not unwind across the FFI boundary.
/// - Whenever `reclaim_fn` returns `true`, it initializes `*out_is_ok` and either `*out_success`
///   (when `*out_is_ok` is `true`) or `*out_failure` (when `*out_is_ok` is `false`).
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_init_with_test_functions(
    evictor: *mut Evictor,
    ctx: *mut core::ffi::c_void,
    reclaim_fn: EvictorTestReclaimFn,
    free_pages_fn: EvictorTestFreePagesFn,
) {
    let ctx = TestCallbackCtx(ctx);
    let reclaim =
        ReclaimFunction::new(move |compression: Option<&'static VmCompression>, eviction_level| {
            let compression_ptr = compression.map_or(core::ptr::null_mut(), |c| c.as_raw().cast());
            let mut is_ok = false;
            let mut success = MaybeUninit::<VmCowReclaimSuccess>::uninit();
            let mut failure = MaybeUninit::<VmCowReclaimFailure>::uninit();
            // SAFETY: The caller of `rust_evictor_init_with_test_functions` guarantees that `ctx`
            // and `reclaim_fn` are valid for the lifetime of `evictor`, and the out-pointers point
            // to local stack variables.
            let has_val = unsafe {
                reclaim_fn(
                    ctx.as_ptr(),
                    compression_ptr,
                    eviction_level,
                    &mut is_ok,
                    success.as_mut_ptr(),
                    failure.as_mut_ptr(),
                )
            };
            if has_val {
                // SAFETY: The caller of `rust_evictor_init_with_test_functions` guarantees that
                // when `reclaim_fn` returns `true`, `is_ok` and the corresponding `success` or
                // `failure` out-parameter have been initialized.
                let res = unsafe {
                    if is_ok { Ok(success.assume_init()) } else { Err(failure.assume_init()) }
                };
                Some(res)
            } else {
                None
            }
        });
    // SAFETY: The caller of `rust_evictor_init_with_test_functions` guarantees that `ctx` and
    // `free_pages_fn` are valid and thread-safe for the lifetime of `evictor`.
    let free_pages = FreePagesFunction::new(move || unsafe { free_pages_fn(ctx.as_ptr()) });
    // SAFETY: The caller guarantees `evictor` points to valid, aligned, uninitialized storage
    // that will remain pinned.
    let _ = unsafe { Evictor::init_for_test(reclaim, free_pages).__pinned_init(evictor) };
}

/// Destroys the `Evictor` at `evictor` in place.
///
/// # Safety
///
/// The caller must ensure that:
/// - `evictor` is a valid, aligned, non-null pointer to a live, pinned `Evictor` that will not be
///   accessed again after this call.
/// - No concurrent calls to `Evictor` methods are active on `evictor`.
/// - The current thread does not hold `evictor`'s internal locks and is not the eviction thread.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_destroy(evictor: *mut Evictor) {
    // SAFETY: The caller guarantees `evictor` points to a live, pinned `Evictor` with no
    // concurrent calls, and shutting down the background eviction thread via shared reference
    // first avoids creating an exclusive `&mut Evictor` while the eviction thread still holds
    // `&Evictor`.
    unsafe {
        (*evictor).disable_eviction();
    }
    // SAFETY: The eviction thread has been joined above, and the caller guarantees `evictor` is
    // valid for dropping in place and will not be used again.
    unsafe {
        core::ptr::drop_in_place(evictor);
    }
}

/// Enables eviction on `evictor`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `evictor` is a valid reference to an initialized `Evictor` that remains pinned and valid
///   until `Evictor::disable_eviction` or `rust_evictor_destroy` completes.
/// - This function is not called concurrently with `enable_eviction`, `disable_eviction`, or
///   `debug_get_evictor_thread` on `evictor`.
/// - The current thread does not hold `evictor.lock`.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_enable_eviction(evictor: &Evictor, use_compression: bool) {
    // SAFETY: The caller satisfies all preconditions of `Evictor::enable_eviction`.
    unsafe {
        evictor.enable_eviction(use_compression);
    }
}

/// Evicts from an external `target` on `evictor`, writing the result to `out_result`.
///
/// # Safety
///
/// The caller must ensure that `evictor` is a valid reference to an initialized `Evictor`,
/// `target` is a valid, aligned, non-null pointer readable for the duration of the call, and
/// `out_result` is a valid, aligned, non-null pointer writable for the duration of the call.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_evict_from_external_target(
    evictor: &Evictor,
    target: *const EvictionTarget,
    out_result: *mut EvictionResult,
) {
    // SAFETY: The caller guarantees `target` is valid for reading and `out_result` is valid for
    // writing.
    unsafe {
        *out_result = evictor.evict_from_external_target(*target);
    }
}

/// Writes the current `EvictionTarget` of `evictor` to `out_target`.
///
/// # Safety
///
/// The caller must ensure that `evictor` is a valid reference to an initialized `Evictor` and
/// `out_target` is a valid, aligned, non-null pointer writable for the duration of the call.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_debug_get_eviction_target(
    evictor: &Evictor,
    out_target: *mut EvictionTarget,
) {
    // SAFETY: The caller guarantees `out_target` is valid for writing.
    unsafe {
        *out_target = evictor.debug_get_eviction_target();
    }
}

/// Combines `target` into `evictor`'s eviction target.
///
/// # Safety
///
/// The caller must ensure that `evictor` is a valid reference to an initialized `Evictor` and
/// `target` is a valid, aligned, non-null pointer readable for the duration of the call.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_combine_eviction_target(
    evictor: &Evictor,
    target: *const EvictionTarget,
) {
    // SAFETY: The caller guarantees `target` is valid for reading.
    unsafe {
        evictor.combine_eviction_target(*target);
    }
}

/// Evicts from the preloaded target on `evictor`, writing the counts to `out_counts`.
///
/// # Safety
///
/// The caller must ensure that `evictor` is a valid reference to an initialized `Evictor` and
/// `out_counts` is a valid, aligned, non-null pointer writable for the duration of the call.
// SAFETY: Symbol name is prefixed with `rust_evictor_` and matches `evictor.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_evictor_evict_from_preloaded_target(
    evictor: &Evictor,
    out_counts: *mut EvictedPageCounts,
) {
    // SAFETY: The caller guarantees `out_counts` is valid for writing.
    unsafe {
        *out_counts = evictor.evict_from_preloaded_target();
    }
}

/// Evictor unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "evictor_rust")]
mod tests {
    use crate::vm::evictor::{
        EvictedPageCounts, EvictionLevel, EvictionTarget, Evictor, FreePagesFunction,
        ReclaimFunction,
    };
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_cow_pages::{VmCowReclaimSuccess, VmCowReclaimType};
    use crate::vm_unittests::test_helper::TestRand;
    use core::convert::Infallible;
    use core::sync::atomic::{AtomicU64, Ordering};
    use pin_init::{PinInit, pin_data, stack_pin_init};
    use rand::Rng;
    use unittest::{assert_eq, expect_eq, expect_ge};

    struct TestPmmNodeState {
        free_pages: AtomicU64,
        total_evictions: AtomicU64,
        max_evictions: AtomicU64,
    }

    #[derive(Copy, Clone)]
    struct StatePtr(*const TestPmmNodeState);

    // SAFETY: `TestPmmNodeState` only contains atomics, so concurrent shared access across threads
    // is sound as long as the pointee remains live.
    unsafe impl Send for StatePtr {}
    // SAFETY: `TestPmmNodeState` only contains atomics, so concurrent shared access across threads
    // is sound as long as the pointee remains live.
    unsafe impl Sync for StatePtr {}

    impl StatePtr {
        /// # Safety
        ///
        /// The caller must ensure that `self.0` points to an initialized `TestPmmNodeState` that
        /// outlives `'a`.
        unsafe fn as_ref<'a>(self) -> &'a TestPmmNodeState {
            // SAFETY: The caller guarantees `self.0` is valid and initialized for `'a`.
            unsafe { &*self.0 }
        }
    }

    /// Custom pmm node to link with the evictor under test. Facilitates verifying the free count
    /// which is not possible with the global pmm node.
    #[pin_data]
    struct TestPmmNode {
        #[pin]
        evictor: Evictor,
        // Safety invariant: `state` is initialized before `evictor` and outlives `evictor`
        // (because struct fields are dropped in declaration order: `evictor` is dropped before
        // `state`). `state` remains pinned for the lifetime of `TestPmmNode`.
        state: TestPmmNodeState,
    }

    impl TestPmmNode {
        fn new(discardable: bool) -> impl PinInit<Self, Infallible> {
            let init = move |slot: *mut Self| {
                // SAFETY: `slot` points to valid, aligned, uninitialized storage for `Self`.
                unsafe {
                    core::ptr::addr_of_mut!((*slot).state).write(TestPmmNodeState {
                        free_pages: AtomicU64::new(0),
                        total_evictions: AtomicU64::new(0),
                        max_evictions: AtomicU64::new(u64::MAX),
                    });
                }
                // SAFETY: `slot` is non-null and we only compute the field address of `state`.
                let state_ptr = StatePtr(unsafe { core::ptr::addr_of!((*slot).state) });
                let reclaim_fn = ReclaimFunction::new(move |_compression, _eviction_level| {
                    // SAFETY: `TestPmmNode` is pinned (`!Unpin`) and `evictor` is declared
                    // before `state`, so `evictor` (and its callbacks) is dropped before
                    // `state` and `state_ptr` remains valid for the lifetime of `reclaim_fn`.
                    let state = unsafe { state_ptr.as_ref() };
                    if state.total_evictions.load(Ordering::Relaxed)
                        >= state.max_evictions.load(Ordering::Relaxed)
                    {
                        return None;
                    }
                    if discardable {
                        // Discardable VMOs get freed in their entirety, which could be any
                        // amount of pages. Claiming 10 here is a bit arbitrary, and could be
                        // made configurable if/when there are some tests that need it.
                        state.free_pages.fetch_add(10, Ordering::Relaxed);
                        state.total_evictions.fetch_add(10, Ordering::Relaxed);
                        return Some(Ok(VmCowReclaimSuccess {
                            type_: VmCowReclaimType::Discard,
                            num_pages: 10,
                            num_loaned_pages: 0,
                        }));
                    }
                    state.free_pages.fetch_add(1, Ordering::Relaxed);
                    state.total_evictions.fetch_add(1, Ordering::Relaxed);
                    Some(Ok(VmCowReclaimSuccess {
                        type_: VmCowReclaimType::Evict,
                        num_pages: 1,
                        num_loaned_pages: 0,
                    }))
                });
                let free_pages_fn = FreePagesFunction::new(move || {
                    // SAFETY: `TestPmmNode` is pinned (`!Unpin`) and `evictor` is dropped
                    // before `state`, so `state_ptr` remains valid for the lifetime of
                    // `free_pages_fn`.
                    let state = unsafe { state_ptr.as_ref() };
                    state.free_pages.load(Ordering::Relaxed)
                });
                // SAFETY: `slot` is non-null and `evictor_slot` points to the uninitialized
                // `evictor` field of `Self`.
                let evictor_slot = unsafe { core::ptr::addr_of_mut!((*slot).evictor) };
                // SAFETY: `evictor_slot` points to valid, aligned, uninitialized storage for
                // `Evictor` that will remain pinned.
                unsafe {
                    Evictor::init_for_test(reclaim_fn, free_pages_fn)
                        .__pinned_init(evictor_slot)?;
                }
                // SAFETY: `evictor_slot` was just initialized above, remains pinned for the
                // lifetime of `TestPmmNode`, and no concurrent calls can occur during init.
                unsafe {
                    (*evictor_slot).enable_eviction(true);
                }
                Ok(())
            };
            // SAFETY: `init` initializes all fields of `Self` (`state` and `evictor`) in place at
            // `slot`.
            unsafe { pin_init::pin_init_from_closure(init) }
        }

        fn get_eviction_target(&self) -> EvictionTarget {
            self.evictor.debug_get_eviction_target()
        }

        fn combine_eviction_target(&self, target: EvictionTarget) {
            self.evictor.combine_eviction_target(target);
        }

        fn evict_from_preloaded_target(&self) -> EvictedPageCounts {
            self.evictor.evict_from_preloaded_target()
        }

        fn free_pages(&self) -> u64 {
            self.state.free_pages.load(Ordering::Relaxed)
        }

        fn evictor(&self) -> &Evictor {
            &self.evictor
        }

        fn cap_evictions(&self, max: u64) {
            self.state.max_evictions.store(max, Ordering::Relaxed);
        }

        fn uncap_evictions(&self) {
            self.state.max_evictions.store(u64::MAX, Ordering::Relaxed);
        }
    }

    /// Test that a one shot eviction target can be set as expected.
    #[test]
    fn evictor_set_target_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(false));
        let mut rng = TestRand::new(12345);

        let expected = EvictionTarget {
            pending: !rng.next_u32().is_multiple_of(2),
            free_pages_target: rng.next_u32() as u64,
            min_pages_to_free: rng.next_u32() as u64,
            level: if !rng.next_u32().is_multiple_of(2) {
                EvictionLevel::IncludeNewest
            } else {
                EvictionLevel::OnlyOldest
            },
            print_counts: false,
            oom_trigger: false,
        };

        node.combine_eviction_target(expected);

        let actual = node.get_eviction_target();

        assert_eq!(actual.pending, expected.pending);
        assert_eq!(actual.free_pages_target, expected.free_pages_target);
        assert_eq!(actual.min_pages_to_free, expected.min_pages_to_free);
        assert_eq!(actual.level, expected.level);
    }

    /// Test that multiple one shot eviction targets can be combined as expected.
    #[test]
    fn evictor_combine_targets_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(false));
        let mut rng = TestRand::new(67890);

        const NUM_TARGETS: usize = 5;
        let mut targets = [EvictionTarget::default(); NUM_TARGETS];

        for target in &mut targets {
            *target = EvictionTarget {
                pending: true,
                free_pages_target: (rng.next_u32() % 1000) as u64,
                min_pages_to_free: (rng.next_u32() % 1000) as u64,
                level: EvictionLevel::IncludeNewest,
                print_counts: false,
                oom_trigger: false,
            };
            node.combine_eviction_target(*target);
        }

        let mut expected = EvictionTarget::default();
        for target in &targets {
            expected.pending = expected.pending || target.pending;
            expected.level = core::cmp::max(expected.level, target.level);
            expected.min_pages_to_free += target.min_pages_to_free;
            expected.free_pages_target =
                core::cmp::max(expected.free_pages_target, target.free_pages_target);
        }

        let actual = node.get_eviction_target();

        assert_eq!(actual.pending, expected.pending);
        assert_eq!(actual.free_pages_target, expected.free_pages_target);
        assert_eq!(actual.min_pages_to_free, expected.min_pages_to_free);
        assert_eq!(actual.level, expected.level);
    }

    /// Test that the evictor can evict from pager backed vmos as expected.
    #[test]
    fn evictor_pager_backed_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(false));

        let mut target = EvictionTarget {
            pending: true,
            free_pages_target: 20,
            min_pages_to_free: 10,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        // The node starts off with zero pages.
        let mut free_count = node.free_pages();
        expect_eq!(free_count, 0);

        node.combine_eviction_target(target);
        let mut counts = node.evict_from_preloaded_target();

        // No discardable pages were evicted.
        expect_eq!(counts.discardable, 0);
        // Free pages target was greater than min pages target. So precisely free pages target must
        // have been evicted.
        expect_eq!(counts.pager_backed, target.free_pages_target);
        expect_ge!(counts.pager_backed, target.min_pages_to_free);
        // The node has the desired number of free pages now, and a minimum of min pages have been
        // freed.
        free_count = node.free_pages();
        expect_eq!(free_count, target.free_pages_target);
        expect_ge!(free_count, target.min_pages_to_free);

        target = EvictionTarget {
            pending: true,
            free_pages_target: 10,
            min_pages_to_free: 20,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No discardable pages were evicted.
        expect_eq!(counts.discardable, 0);
        // Min pages target was greater than free pages target. So precisely min pages target must
        // have been evicted.
        expect_eq!(counts.pager_backed, target.min_pages_to_free);
        // The node has the desired number of free pages now, and a minimum of min pages have been
        // freed.
        expect_ge!(node.free_pages(), target.free_pages_target);
        expect_eq!(node.free_pages(), free_count + target.min_pages_to_free);
    }

    /// Test that the evictor can discard from discardable vmos as expected.
    #[test]
    fn evictor_discardable_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(true));

        let mut target = EvictionTarget {
            pending: true,
            free_pages_target: 20,
            min_pages_to_free: 10,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        // The node starts off with zero pages.
        let mut free_count = node.free_pages();
        expect_eq!(free_count, 0);

        node.combine_eviction_target(target);
        let mut counts = node.evict_from_preloaded_target();

        // No pager backed pages were evicted.
        expect_eq!(counts.pager_backed, 0);
        // Free pages target was greater than min pages target. So precisely free pages target must
        // have been evicted. However, a discardable vmo can only be discarded in its
        // entirety, so we can't check for equality with free pages target.
        expect_ge!(counts.discardable, target.free_pages_target);
        expect_ge!(counts.discardable, target.min_pages_to_free);
        // The node has the desired number of free pages now, and a minimum of min pages have been
        // freed.
        free_count = node.free_pages();
        expect_ge!(free_count, target.free_pages_target);
        expect_ge!(free_count, target.min_pages_to_free);

        target = EvictionTarget {
            pending: true,
            free_pages_target: 10,
            min_pages_to_free: 20,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No pager backed pages were evicted.
        expect_eq!(counts.pager_backed, 0);
        // Min pages target was greater than free pages target. So precisely min pages target must
        // have been evicted. However, a discardable vmo can only be discarded in its
        // entirety, so we can't check for equality with free pages target.
        expect_ge!(counts.discardable, target.min_pages_to_free);
        // The node has the desired number of free pages now, and a minimum of min pages have been
        // freed.
        expect_ge!(node.free_pages(), target.free_pages_target);
        expect_ge!(node.free_pages(), free_count + target.min_pages_to_free);
    }

    /// Test that eviction meets the required free and min target as expected.
    #[test]
    fn evictor_free_target_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        // Only evict from pager backed vmos.
        stack_pin_init!(let node = TestPmmNode::new(false));

        let mut target = EvictionTarget {
            pending: true,
            free_pages_target: 20,
            min_pages_to_free: 0,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        // The node starts off with zero pages.
        let mut free_count = node.free_pages();
        expect_eq!(free_count, 0);

        node.combine_eviction_target(target);
        let mut counts = node.evict_from_preloaded_target();

        // No discardable pages were evicted.
        expect_eq!(counts.discardable, 0);
        // Free pages target was greater than min pages target. So precisely free pages target must
        // have been evicted.
        expect_eq!(counts.pager_backed, target.free_pages_target);
        // The node has the desired number of free pages now, and a minimum of min pages have been
        // freed.
        free_count = node.free_pages();
        expect_eq!(free_count, target.free_pages_target);
        expect_ge!(free_count, target.min_pages_to_free);

        // Evict again with the same target.
        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No new pages should have been evicted, as the free target was already met with the
        // previous round of eviction, and no minimum pages were requested to be evicted.
        expect_eq!(counts.discardable, 0);
        expect_eq!(counts.pager_backed, 0);
        expect_eq!(node.free_pages(), free_count);

        // Evict again with a higher free memory target. No min pages target.
        let delta_pages = 10;
        target.free_pages_target += delta_pages;
        target.min_pages_to_free = 0;
        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No discardable pages evicted.
        expect_eq!(counts.discardable, 0);
        // Exactly delta_pages evicted.
        expect_eq!(counts.pager_backed, delta_pages);
        expect_ge!(counts.pager_backed, target.min_pages_to_free);
        // Free count increased by delta_pages.
        free_count = node.free_pages();
        expect_eq!(free_count, target.free_pages_target);

        // Evict again with a higher free memory target and also a min pages target.
        target.free_pages_target += delta_pages;
        target.min_pages_to_free = delta_pages;
        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No discardable pages evicted.
        expect_eq!(counts.discardable, 0);
        // Exactly delta_pages evicted.
        expect_eq!(counts.pager_backed, delta_pages);
        expect_ge!(counts.pager_backed, target.min_pages_to_free);
        // Free count increased by delta_pages.
        free_count = node.free_pages();
        expect_eq!(free_count, target.free_pages_target);

        // Evict again with the same free target, but request a min number of pages to be freed.
        target.min_pages_to_free = 2;
        node.combine_eviction_target(target);
        counts = node.evict_from_preloaded_target();

        // No discardable pages evicted.
        expect_eq!(counts.discardable, 0);
        // Exactly min pages evicted.
        expect_eq!(counts.pager_backed, target.min_pages_to_free);
        // Free count increased by min pages.
        expect_eq!(node.free_pages(), free_count + target.min_pages_to_free);
    }

    /// Test that eviction using an external target does not alter a preloaded eviction target.
    #[test]
    fn evictor_external_target_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(false));
        let mut rng = TestRand::new(54321);

        let expected = EvictionTarget {
            pending: !rng.next_u32().is_multiple_of(2),
            free_pages_target: 111,
            min_pages_to_free: 33,
            level: if !rng.next_u32().is_multiple_of(2) {
                EvictionLevel::IncludeNewest
            } else {
                EvictionLevel::OnlyOldest
            },
            print_counts: false,
            oom_trigger: false,
        };

        node.combine_eviction_target(expected);

        let external = EvictionTarget {
            pending: !expected.pending,
            free_pages_target: 99,
            min_pages_to_free: 22,
            level: if expected.level == EvictionLevel::OnlyOldest {
                EvictionLevel::IncludeNewest
            } else {
                EvictionLevel::OnlyOldest
            },
            print_counts: false,
            oom_trigger: false,
        };
        let _ = node.evictor().evict_from_external_target(external);

        let actual = node.get_eviction_target();

        assert_eq!(actual.pending, expected.pending);
        assert_eq!(actual.free_pages_target, expected.free_pages_target);
        assert_eq!(actual.min_pages_to_free, expected.min_pages_to_free);
        assert_eq!(actual.level, expected.level);
    }

    /// Test that a one shot eviction target can be set as expected.
    #[test]
    fn evictor_min_target_carried_over_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        stack_pin_init!(let node = TestPmmNode::new(false));

        let target = EvictionTarget {
            pending: true,
            free_pages_target: 10,
            min_pages_to_free: 15,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        };

        // The node starts off with zero pages.
        let mut free_count = node.free_pages();
        expect_eq!(free_count, 0);

        // Cap the number of evictions to 5.
        node.cap_evictions(5);

        node.combine_eviction_target(target);
        let mut counts = node.evict_from_preloaded_target();

        // No discardable pages evicted.
        expect_eq!(counts.discardable, 0);
        // Exactly 5 pages evicted.
        expect_eq!(counts.pager_backed, 5);

        // Uncap evictions.
        node.uncap_evictions();

        // Combine target with zero min pages requested.
        node.combine_eviction_target(EvictionTarget {
            pending: true,
            free_pages_target: 0,
            min_pages_to_free: 0,
            level: EvictionLevel::IncludeNewest,
            print_counts: false,
            oom_trigger: false,
        });
        counts = node.evict_from_preloaded_target();

        // No discardable pages evicted.
        expect_eq!(counts.discardable, 0);
        // Remaining pages should have been evicted.
        expect_eq!(counts.pager_backed, target.min_pages_to_free - 5);

        free_count = node.free_pages();
        expect_eq!(free_count, target.min_pages_to_free);
    }
}
