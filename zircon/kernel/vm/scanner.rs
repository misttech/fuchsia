// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::backtrace::Backtrace;
use crate::counters::define_kcounter;
use crate::kernel::deadline::{Deadline, InstantUnknown};
use crate::kernel::event::{AutounsignalEvent, Event};
use crate::kernel::thread::{self, LOW_PRIORITY, ThreadPtr};
use crate::object::get_memory_watchdog;
use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
use crate::vm::arch_vm_aspace::{NonTerminalAction, TerminalAction};
use crate::vm::compression::VmCompression;
use crate::vm::discardable_vmo_tracker::DiscardableVmoTracker;
use crate::vm::evictor::{EvictionLevel, EvictionTarget};
use crate::vm::page_queues::{self, LruAction, PageQueues};
use crate::vm::pmm;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_cow_pages::VmCowPages;
use boot_options::{BootOptions, PageTableEvictionPolicy, ScannerLruAction};
use core::ffi::c_void;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use kprint::kprintln;
use ksync::{KCell, LockToken, declare_singleton_mutex, lock};
use lazy_init::LazyInit;

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
#[cfg(console_enabled)]
use crate::vm::evictor::Output;
#[cfg(console_enabled)]
use zx_status::Status;

const SCANNER_FLAG_PRINT: u32 = 1 << 0;
const SCANNER_OP_DISABLE: u32 = 1 << 1;
const SCANNER_OP_ENABLE: u32 = 1 << 2;
const SCANNER_OP_DUMP: u32 = 1 << 3;
const SCANNER_OP_RECLAIM_ALL: u32 = 1 << 4;
const SCANNER_OP_UPDATE_HARVEST_TIME: u32 = 1 << 6;
const SCANNER_OP_ENABLE_PT_RECLAIM: u32 = 1 << 8;
const SCANNER_OP_DISABLE_PT_RECLAIM: u32 = 1 << 9;

/// Amount of time between page table evictions. This is not atomic as it is only set during init
/// before the scanner thread starts up, at which point it becomes read only.
static PAGE_TABLE_EVICT_TIME: LazyInit<DurationMono> = LazyInit::uninit();

/// Number of pages to attempt to de-dupe back to zero every second. This not atomic as it is only
/// set during init before the scanner thread starts up, at which point it becomes read only.
static ZERO_PAGE_SCANS_PER_SECOND: LazyInit<u64> = LazyInit::uninit();

static PAGE_TABLE_RECLAIM_POLICY: LazyInit<PageTableEvictionPolicy> = LazyInit::uninit();

/// Tracks what the scanner should do when it is next woken up.
static SCANNER_OPERATION: AtomicU32 = AtomicU32::new(0);

/// Event to signal the scanner thread to wake up and perform work.
static SCANNER_REQUEST_EVENT: LazyInit<AutounsignalEvent> = LazyInit::uninit();

/// Event that is signaled whenever the scanner is disabled. This is used to synchronize disable
/// requests with the scanner thread.
static SCANNER_DISABLED_EVENT: LazyInit<Event> = LazyInit::uninit();
declare_singleton_mutex!(ScannerDisabledLock);
static SCANNER_DISABLE_COUNT: KCell<u32, ScannerDisabledLock> = KCell::new(0);

/// This variable needs to be protected by a mutex because it may be accessed by
/// `push_disable_count` or `pop_disable_count` concurrently with
/// `scanner_init_func`. We're reusing the `ScannerDisabledLock` because the push and pop
/// functions already acquire it.
static SCANNER_THREAD: KCell<Option<ThreadPtr>, ScannerDisabledLock> = KCell::new(None);

declare_singleton_mutex!(
    /// Mutex used to ensure only a single access scan is happening at once.
    AccessedScannerLock
);
static LAST_ACCESSED_SCAN_COMPLETE: AtomicI64 = AtomicI64::new(InstantMono::INFINITE_PAST.0);

/// The accessed scan rate starts matched to the minimum aging period, since scanning more
/// frequently than that does not produce any fidelity of information.
static ACCESSED_SCAN_PERIOD: AtomicI64 = AtomicI64::new(page_queues::DEFAULT_MIN_MRU_ROTATE_TIME.0);

static RECLAIM_PT_NEXT_ACCESSED_SCAN: AtomicBool = AtomicBool::new(false);

define_kcounter!(ZERO_SCAN_REQUESTS, "vm.scanner.zero_scan.requests", Sum);
define_kcounter!(ZERO_SCAN_ENDS_EMPTY, "vm.scanner.zero_scan.queue_emptied", Sum);
define_kcounter!(ZERO_SCAN_PAGES_SCANNED, "vm.scanner.zero_scan.total_pages_considered", Sum);
define_kcounter!(ZERO_SCAN_PAGES_DEDUPED, "vm.scanner.zero_scan.pages_deduped", Sum);

fn scanner_print_stats() {
    let queue_counts = pmm::page_queues().queue_counts();
    for (i, &count) in queue_counts.reclaim.iter().enumerate().take(PageQueues::NUM_RECLAIM) {
        kprintln!("[SCAN]: Found {} reclaimable pages in queue {}", count, i);
    }
    kprintln!("[SCAN]: Found {} failed reclaim pages", queue_counts.failed_reclaim);
    kprintln!("[SCAN]: Found {} dirty pager backed pages", queue_counts.pager_backed_dirty);
    kprintln!("[SCAN]: Found {} zero forked pages", queue_counts.anonymous_zero_fork);

    let counts = DiscardableVmoTracker::debug_discardable_page_counts();
    kprintln!("[SCAN]: Found {} locked pages in discardable vmos", counts.locked);
    kprintln!("[SCAN]: Found {} unlocked pages in discardable vmos", counts.unlocked);
    pmm::page_queues().dump();
    if let Some(compression) = pmm::node().get_page_compression() {
        compression.dump();
    }
}

fn calc_next_zero_scan_deadline(current: InstantMono) -> InstantMono {
    if *ZERO_PAGE_SCANS_PER_SECOND > 0 {
        current + DurationMono::from_seconds(1)
    } else {
        InstantMono::INFINITE
    }
}

fn calc_next_pt_evict_deadline(current: InstantMono, pt_enable_override: bool) -> InstantMono {
    if *PAGE_TABLE_RECLAIM_POLICY == PageTableEvictionPolicy::Always || pt_enable_override {
        current + *PAGE_TABLE_EVICT_TIME
    } else {
        InstantMono::INFINITE
    }
}

extern "C" fn scanner_request_thread(_arg: *mut c_void) -> i32 {
    let mut disabled = false;
    let mut pt_eviction_enabled = false;
    let mut last_pt_evict = InstantMono::INFINITE_PAST;
    let mut next_zero_scan_deadline = calc_next_zero_scan_deadline(current_mono_time());
    let mut next_harvest_deadline =
        current_mono_time() + DurationMono(ACCESSED_SCAN_PERIOD.load(Ordering::SeqCst));
    loop {
        if disabled {
            let _ = SCANNER_REQUEST_EVENT.wait(&Deadline::infinite());
        } else {
            let next_pt_evict_deadline =
                calc_next_pt_evict_deadline(last_pt_evict, pt_eviction_enabled);
            let _ = SCANNER_REQUEST_EVENT.wait(&Deadline::no_slack(InstantUnknown::from(
                core::cmp::min(
                    next_pt_evict_deadline,
                    core::cmp::min(next_zero_scan_deadline, next_harvest_deadline),
                ),
            )));
        }
        let mut op = SCANNER_OPERATION.swap(0, Ordering::SeqCst);
        // It is possible for enable and disable to happen at the same time. This indicates the
        // disabled count went from 1->0->1 and so we want to remain disabled. We do this by
        // performing the enable step first. We know that the scenario of 0->1->0 is not possible as
        // the 0->1 part of that holds the mutex until complete.
        if (op & SCANNER_OP_ENABLE) != 0 {
            op &= !SCANNER_OP_ENABLE;
            pmm::page_queues().enable_aging();
            // Re-enable eviction if it was originally enabled.
            if BootOptions::get().page_scanner_enable_eviction {
                // SAFETY: `pmm::evictor()` is a static pinned `Evictor` and the scanner thread
                // serializes all `enable_eviction` and `disable_eviction` calls.
                unsafe {
                    pmm::evictor()
                        .enable_eviction(BootOptions::get().compression_at_memory_pressure);
                }
            }
            disabled = false;
        }
        if (op & SCANNER_OP_DISABLE) != 0 {
            op &= !SCANNER_OP_DISABLE;
            // Make sure no eviction is happening either.
            // SAFETY: The scanner thread serializes `enable_eviction` and `disable_eviction` calls
            // and is not the eviction thread.
            unsafe {
                pmm::evictor().disable_eviction();
            }
            disabled = true;
            pmm::page_queues().disable_aging();
            // Grab the harvester lock to wait for any in progress scans to complete.
            {
                lock!(let _guard = AccessedScannerLock::lock());
            }
            SCANNER_DISABLED_EVENT.signal();
        }
        if disabled {
            // put the remaining ops back and resume waiting.
            SCANNER_OPERATION.fetch_or(op, Ordering::SeqCst);
            continue;
        }

        let current = current_mono_time();

        if current >= calc_next_pt_evict_deadline(last_pt_evict, pt_eviction_enabled)
            || (op & SCANNER_OP_RECLAIM_ALL) != 0
        {
            // Make sure a scan has happened since we last expected page table reclamation to
            // happen. This in effect will cause scanning to happen at least once every pt
            // reclamation period, and therefore for reclamation to happen, on average, once every
            // target period. This is fine, and the goal of this is to ensure that we avoid
            // triggering additional accessed scans if we can avoid it, and that we additionally do
            // not reclaim page tables too often.
            wait_for_accessed_scan(last_pt_evict);
            // Trigger pt eviction to happen next time, which in the worst case will be once we
            // timeout and call `wait_for_accessed_scan` above. In essence this is introducing some
            // slack to the reclamation timeout to maximize the chance that the reclamation gets
            // paired with a separate accessed harvest.
            RECLAIM_PT_NEXT_ACCESSED_SCAN.store(true, Ordering::SeqCst);
            // Set now to our last pt evict so we retry again next period.
            last_pt_evict = current;
        }

        // Recalculate `next_harvest_deadline`, as scanning may have happened via a different means
        // while we were sleeping and we do not want to re-scan unless *no* scan happens for the
        // period.
        next_harvest_deadline = InstantMono(LAST_ACCESSED_SCAN_COMPLETE.load(Ordering::SeqCst))
            + DurationMono(ACCESSED_SCAN_PERIOD.load(Ordering::SeqCst));
        if current >= next_harvest_deadline {
            wait_for_accessed_scan(next_harvest_deadline);
            op |= SCANNER_OP_UPDATE_HARVEST_TIME;
        }

        let mut print = false;
        if (op & SCANNER_FLAG_PRINT) != 0 {
            op &= !SCANNER_FLAG_PRINT;
            print = true;
        }
        let mut reclaim_all = false;
        if (op & SCANNER_OP_RECLAIM_ALL) != 0 {
            op &= !SCANNER_OP_RECLAIM_ALL;
            reclaim_all = true;
            let target = EvictionTarget {
                pending: true,
                free_pages_target: u64::MAX,
                level: EvictionLevel::IncludeNewest,
                print_counts: print,
                ..EvictionTarget::default()
            };
            pmm::evictor().evict_from_external_target(target);
            // To ensure any page table eviction that was set earlier actually occurs, force an
            // accessed scan to happen right now.
            wait_for_accessed_scan(current_mono_time());
        }
        if (op & SCANNER_OP_DUMP) != 0 {
            op &= !SCANNER_OP_DUMP;
            scanner_print_stats();
        }
        if (op & SCANNER_OP_ENABLE_PT_RECLAIM) != 0 {
            pt_eviction_enabled = true;
            op &= !SCANNER_OP_ENABLE_PT_RECLAIM;
        }
        if (op & SCANNER_OP_DISABLE_PT_RECLAIM) != 0 {
            pt_eviction_enabled = false;
            op &= !SCANNER_OP_DISABLE_PT_RECLAIM;
        }
        if (op & SCANNER_OP_UPDATE_HARVEST_TIME) != 0 {
            op &= !SCANNER_OP_UPDATE_HARVEST_TIME;
            next_harvest_deadline = InstantMono(LAST_ACCESSED_SCAN_COMPLETE.load(Ordering::SeqCst))
                + DurationMono(ACCESSED_SCAN_PERIOD.load(Ordering::SeqCst));
        }
        if current >= next_zero_scan_deadline || reclaim_all {
            let scan_limit = if reclaim_all { u64::MAX } else { *ZERO_PAGE_SCANS_PER_SECOND };
            let pages = do_zero_scan(scan_limit);
            if print {
                kprintln!(
                    "[SCAN]: De-duped {} pages that were recently forked from the zero page",
                    pages
                );
            }
            next_zero_scan_deadline = calc_next_zero_scan_deadline(current);
        }
        debug_assert!(op == 0);
    }
}

fn scanner_dump_info() {
    lock!(let guard = ScannerDisabledLock::lock());
    // SAFETY: `ScannerDisabledLock` is a singleton mutex and `guard.token()` proves it is held.
    let disable_count = unsafe { *SCANNER_DISABLE_COUNT.get(guard.token()) };
    if disable_count > 0 {
        kprintln!("[SCAN]: Scanner disabled with disable count of {}", disable_count);
    } else {
        kprintln!("[SCAN]: Scanner enabled. Triggering informational scan");
        SCANNER_OPERATION.fetch_or(SCANNER_OP_DUMP, Ordering::SeqCst);
        SCANNER_REQUEST_EVENT.signal();
    }
}

/// Returns true if no accessed scan has completed since `update_time`. If this returns false then
/// calling [`wait_for_accessed_scan`] would be a no-op. If this returns true then calling
/// [`wait_for_accessed_scan`] may or may not block to perform a new scan.
pub(super) fn needs_accessed_scan(update_time: InstantMono) -> bool {
    update_time.0 > LAST_ACCESSED_SCAN_COMPLETE.load(Ordering::SeqCst)
}

/// Blocks until the scanner has completed an access scan that occurred at `update_time` or later.
/// This means if an accessed scan already happened more recently this function will immediately
/// return, otherwise it will wait for a new scan to complete.
///
/// Currently accessed scanning happens completely inline, and so this does one of three things
/// 1. Returns immediately if a sufficiently recent scan already happened
/// 2. Waits for an in progress scan to finish, and then most likely returns unless `update_time` is
///    `InstantMono::INFINITE`
/// 3. Performs an entire scan and then returns.
///
/// The public definition of this method is abstract to allow for this to, in the future, not
/// necessarily perform a scan itself, but sync up with the scanner thread that might be slowly
/// scanning in the background.
pub(super) fn wait_for_accessed_scan(update_time: InstantMono) {
    if !needs_accessed_scan(update_time) {
        // scanning is sufficiently up to date.
        return;
    }
    lock!(let _guard = AccessedScannerLock::lock());
    // Re-check now that we hold the lock in case a scan just finished and we were blocked on it.
    if !needs_accessed_scan(update_time) {
        return;
    }
    let reclaim_pt = RECLAIM_PT_NEXT_ACCESSED_SCAN.swap(false, Ordering::SeqCst);
    // Perform a scan.
    // If we neither have page eviction or page table eviction then we can skip harvesting
    // accessed bits.
    if reclaim_pt || pmm::evictor().is_eviction_enabled() {
        let action =
            if reclaim_pt { NonTerminalAction::FreeUnaccessed } else { NonTerminalAction::Retain };
        VmAspace::harvest_all_user_accessed_bits(action, TerminalAction::UpdateAgeAndHarvest);
    }
    LAST_ACCESSED_SCAN_COMPLETE.store(current_mono_time().0, Ordering::SeqCst);
}

/// Attempts to scan for, and dedupe, zero pages. Page candidates are pulled from the
/// `anonymous_zero_fork` page queue. It will consider up to `limit` candidates, and return the
/// number of pages actually deduped.
/// This is expected to be used internally by the scanner thread, but is exposed for testing,
/// debugging and other code to use.
fn do_zero_scan(limit: u64) -> u64 {
    let mut deduped = 0;
    let mut considered = 0;
    ZERO_SCAN_REQUESTS.add(1);
    while considered < limit {
        if let Some(backlink) = pmm::page_queues().pop_anonymous_zero_fork() {
            considered += 1;
            if let Some(cow) = backlink.cow.as_ref()
                && cow.dedup_zero_page(backlink.page, backlink.offset)
            {
                deduped += 1;
            }
        } else {
            ZERO_SCAN_ENDS_EMPTY.add(1);
            break;
        }
    }

    ZERO_SCAN_PAGES_SCANNED.add(considered as i64);
    ZERO_SCAN_PAGES_DEDUPED.add(deduped as i64);
    deduped
}

/// Sets the scanner to reclaim page tables when harvesting accessed bits in the future, unless
/// page table reclamation was explicitly disabled on the command line. Repeatedly enabling does not
/// stack.
fn enable_page_table_reclaim() {
    if *PAGE_TABLE_RECLAIM_POLICY != PageTableEvictionPolicy::OnRequest {
        return;
    }
    SCANNER_OPERATION.fetch_or(SCANNER_OP_ENABLE_PT_RECLAIM, Ordering::SeqCst);
    SCANNER_REQUEST_EVENT.signal();
}

/// Inverse of [`enable_page_table_reclaim`], also does not stack.
fn disable_page_table_reclaim() {
    if *PAGE_TABLE_RECLAIM_POLICY != PageTableEvictionPolicy::OnRequest {
        return;
    }
    SCANNER_OPERATION.fetch_or(SCANNER_OP_DISABLE_PT_RECLAIM, Ordering::SeqCst);
    SCANNER_REQUEST_EVENT.signal();
}

/// Increase the disable count of the scanner. This may need to block until the scanner finishes any
/// current work and so should not be called with other locks held that may conflict with the
/// scanner. Generally this is expected to be used by unittests.
pub fn push_disable_count() {
    lock!(let mut guard = ScannerDisabledLock::lock());
    let token = guard.token_mut();
    // Make sure we have a scanner thread, otherwise the wait may block forever.
    // SAFETY: `ScannerDisabledLock` is a singleton mutex and `token` proves it is held.
    unsafe {
        debug_assert!(SCANNER_THREAD.get(token).is_some());
        if *SCANNER_DISABLE_COUNT.get(token) == 0 {
            SCANNER_OPERATION.fetch_or(SCANNER_OP_DISABLE, Ordering::SeqCst);
            SCANNER_REQUEST_EVENT.signal();
        }
        *SCANNER_DISABLE_COUNT.get_mut(token) += 1;
    }
    let _ = SCANNER_DISABLED_EVENT.wait(&Deadline::infinite());
}

/// Decrease the disable count of the scanner, potentially re-enabling the scanner if it reaches
/// zero.
pub fn pop_disable_count() {
    lock!(let mut guard = ScannerDisabledLock::lock());
    let token = guard.token_mut();
    // SAFETY: `ScannerDisabledLock` is a singleton mutex and `token` proves it is held.
    unsafe {
        debug_assert!(SCANNER_THREAD.get(token).is_some());
        debug_assert!(*SCANNER_DISABLE_COUNT.get(token) > 0);
        *SCANNER_DISABLE_COUNT.get_mut(token) -= 1;
        if *SCANNER_DISABLE_COUNT.get(token) == 0 {
            SCANNER_OPERATION.fetch_or(SCANNER_OP_ENABLE, Ordering::SeqCst);
            SCANNER_REQUEST_EVENT.signal();
            let _ = SCANNER_DISABLED_EVENT.unsignal();
        }
    }
}

/// `AutoVmScannerDisable` is an RAII helper for disabling scanning using the
/// [`push_disable_count()`]/[`pop_disable_count()`]. Disabling the scanner is useful in test
/// code where it is not possible or practical to hold locks to prevent the scanner from taking
/// actions.
#[must_use]
pub struct AutoVmScannerDisable(());

impl AutoVmScannerDisable {
    /// Creates a new `AutoVmScannerDisable` guard, disabling the VM scanner.
    pub fn new() -> Self {
        push_disable_count();
        Self(())
    }
}

impl Default for AutoVmScannerDisable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoVmScannerDisable {
    fn drop(&mut self) {
        pop_disable_count();
    }
}

/// Ask the scanner to dump informational state before issuing a panic. The assumption on calling
/// this is that the panic is due to eviction/scanner going wrong for some hard to infer reason at
/// the panic site, and this method can attempt to provide some additional context in the kernel
/// log. This should only be called if a panic is expected as this performs semi-unsafe operations
/// that might themselves crash or panic the kernel.
///
/// # Safety
///
/// Must only be called immediately prior to a kernel panic when not racing with fundamental state
/// changes or destruction of the threads being inspected, and while not holding any of their
/// thread locks or an active chain lock transaction.
pub unsafe fn debug_dump_state_before_panic() {
    VmCowPages::debug_dump_reclaim_counters();
    // Retrieve a reference to any possible eviction related threads.
    // SAFETY: Caller guarantees we are dumping state immediately before a panic and not racing with
    // eviction or scanner thread teardown.
    let threads = [
        get_memory_watchdog().debug_get_worker_thread(),
        unsafe { pmm::node().evictor().debug_get_evictor_thread() },
        unsafe { pmm::node().page_queues().debug_get_mru_thread() },
        unsafe { pmm::node().page_queues().debug_get_lru_thread() },
        unsafe { *SCANNER_THREAD.get(&LockToken::new()) },
    ];
    // Attempt to dump information and backtraces for all the threads.
    for thread in threads {
        let Some(thread) = thread else {
            // The given thread may not exist due to current system state or configuration, if so
            // just skip it.
            continue;
        };
        // This is the unsafe part where we assume that we are not racing with any fundamental state
        // changes to the system we received these threads from, and hence they are still valid
        // Thread objects.
        let mut backtrace = Backtrace::new();
        // SAFETY: Caller guarantees `thread` remains a valid `Thread` during the pre-panic dump.
        unsafe {
            thread.get_backtrace(&mut backtrace);
            thread.dump(true);
        }
        backtrace.print();
    }
}

fn scanner_init_func(_level: init::LkInitLevel) {
    // SAFETY: `scanner_init_func` runs once during single-threaded boot before the scanner thread
    // is resumed or any external callers access these events.
    unsafe {
        let _ =
            Pin::static_ref(&SCANNER_REQUEST_EVENT).init_pin(AutounsignalEvent::init_unsignaled());
        let _ = Pin::static_ref(&SCANNER_DISABLED_EVENT).init_pin(Event::init_unsignaled());
    }
    // SAFETY: `scanner_request_thread` is a valid thread entry point with no argument.
    let thread = unsafe {
        thread::create_with_priority(
            c"scanner-request-thread".as_ptr(),
            scanner_request_thread,
            ptr::null_mut(),
            LOW_PRIORITY,
        )
    };
    debug_assert!(thread.is_ok());
    let thread = thread.unwrap();
    // SAFETY: `ZERO_PAGE_SCANS_PER_SECOND` is initialized once during boot before `thread` is
    // resumed.
    unsafe {
        ZERO_PAGE_SCANS_PER_SECOND.init(BootOptions::get().page_scanner_zero_page_scans_per_second);
    }
    if !BootOptions::get().page_scanner_start_at_boot {
        lock!(let mut guard = ScannerDisabledLock::lock());
        // SAFETY: `ScannerDisabledLock` is a singleton mutex and `guard.token_mut()` proves it is
        // held.
        unsafe {
            *SCANNER_DISABLE_COUNT.get_mut(guard.token_mut()) += 1;
        }
        SCANNER_OPERATION.fetch_or(SCANNER_OP_DISABLE, Ordering::SeqCst);
        SCANNER_REQUEST_EVENT.signal();
    }
    // SAFETY: `PAGE_TABLE_RECLAIM_POLICY` and `PAGE_TABLE_EVICT_TIME` are initialized once during
    // boot before `thread` is resumed.
    unsafe {
        PAGE_TABLE_RECLAIM_POLICY.init(BootOptions::get().page_scanner_page_table_eviction_policy);
        PAGE_TABLE_EVICT_TIME.init(core::cmp::max(
            DurationMono::from_millis(
                BootOptions::get().page_scanner_page_table_eviction_period_ms as i64,
            ),
            DurationMono::from_seconds(1),
        ));
    }

    if BootOptions::get().page_scanner_enable_eviction {
        // SAFETY: `pmm::evictor()` is a static pinned `Evictor` and `scanner_init_func` runs once
        // during boot.
        unsafe {
            pmm::evictor().enable_eviction(BootOptions::get().compression_at_memory_pressure);
        }
    }

    pmm::page_queues()
        .set_active_ratio_multiplier(BootOptions::get().page_scanner_active_ratio_multiplier);
    // SAFETY: `scanner_init_func` runs once during boot, so `start_threads` is called only once on
    // the static pinned `pmm::page_queues()`.
    unsafe {
        pmm::page_queues().start_threads(
            DurationMono::from_millis(BootOptions::get().page_scanner_min_aging_interval_ms as i64),
            DurationMono::from_millis(BootOptions::get().page_scanner_max_aging_interval_ms as i64),
        );
    }
    // Set the access scan to at least 1 second over the min page scanning interval. This both
    // ensures that the access scan period is never 0 and that redundant scanning before the page
    // queues can age does not occur.
    let accessed_scan_period = DurationMono::from_millis(core::cmp::max(
        BootOptions::get().page_scanner_min_aging_interval_ms + 1000,
        BootOptions::get().page_scanner_accessed_scan_interval_ms,
    ) as i64);
    ACCESSED_SCAN_PERIOD.store(accessed_scan_period.0, Ordering::SeqCst);

    if let Some(compression) = VmCompression::create_default() {
        let status = pmm::node().set_page_compression(compression);
        assert!(status.is_ok(), "status: {}", status.err().unwrap().into_raw());
        if BootOptions::get().random_debug_compress {
            pmm::page_queues().start_debug_compressor();
            kprintln!("scanner: kernel.compression.random-debug-compress enabled");
        }
    } else {
        // It is an error to set the random-debug-compress option if no compression was specified.
        assert!(!BootOptions::get().random_debug_compress);
    }

    if BootOptions::get().compression_reclaim_anonymous {
        pmm::page_queues()
            .enable_anonymous_reclaim(BootOptions::get().compression_reclaim_zero_forks);
    } else {
        assert!(!BootOptions::get().compression_reclaim_zero_forks);
    }
    // Convert the boot option `lru_action` to the page queues one. These are currently the same
    // set, but we use different types to preserve the layering.
    let lru_action = match BootOptions::get().lru_action {
        ScannerLruAction::None => LruAction::None,
        ScannerLruAction::EvictOnly => LruAction::EvictOnly,
        ScannerLruAction::CompressOnly => LruAction::CompressOnly,
        ScannerLruAction::EvictAndCompress => LruAction::EvictAndCompress,
    };
    pmm::page_queues().set_lru_action(lru_action);
    // SAFETY: `thread` was just created above and has not been resumed or joined yet.
    unsafe {
        thread.resume();
    }

    {
        lock!(let mut guard = ScannerDisabledLock::lock());
        let token = guard.token_mut();
        // SAFETY: `ScannerDisabledLock` is a singleton mutex and `token` proves it is held.
        unsafe {
            debug_assert!(SCANNER_THREAD.get(token).is_none());
            *SCANNER_THREAD.get_mut(token) = Some(thread);
        }
    }
}

// Start the scanner before the kernel shell and userspace since they may interact with and depend
// on the scanner running.
init::lk_init_hook!(
    scanner_init,
    scanner_init_func,
    init::LkInitLevel(init::LK_INIT_LEVEL_USER.0 - 1)
);

/// # Safety
///
/// `argc` must be `>= 1` and `argv` must be non-null and point to `argc` valid `CmdArgs`
/// structures.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_scanner(
    argc: core::ffi::c_int,
    argv: *const CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    // SAFETY: The caller guarantees `argc >= 1` and `argv` is non-null and points to `argc`
    // elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> core::ffi::c_int {
        kprintln!("not enough arguments");
        kprintln!("usage:");
        kprintln!("{:s} dump                    : dump scanner info", argv[0].as_str());
        kprintln!(
            "{:s} push_disable            : increase scanner disable count",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} pop_disable             : decrease scanner disable count",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} reclaim_all             : attempt to reclaim all possible memory",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} rotate_queue            : immediately rotate the page queues",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} reclaim <MB> [only_old] : attempt to reclaim requested MB of memory.",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} pt_reclaim [on|off]     : turn unused page table reclamation on or off",
            argv[0].as_str()
        );
        kprintln!(
            "{:s} harvest_accessed        : harvest all page accessed information",
            argv[0].as_str()
        );
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        return usage();
    }

    if argv[1].as_str() == "dump" {
        scanner_dump_info();
    } else if argv[1].as_str() == "push_disable" {
        push_disable_count();
    } else if argv[1].as_str() == "pop_disable" {
        pop_disable_count();
    } else if argv[1].as_str() == "reclaim_all" {
        SCANNER_OPERATION.fetch_or(SCANNER_OP_RECLAIM_ALL | SCANNER_FLAG_PRINT, Ordering::SeqCst);
        SCANNER_REQUEST_EVENT.signal();
    } else if argv[1].as_str() == "rotate_queue" {
        pmm::page_queues().rotate_reclaim_queues();
    } else if argv[1].as_str() == "harvest_accessed" {
        wait_for_accessed_scan(current_mono_time());
    } else if argv[1].as_str() == "reclaim" {
        if argc < 3 {
            return usage();
        }
        if !pmm::evictor().is_eviction_enabled() {
            kprintln!(
                "{:s} is false, reclamation request will have no effect",
                boot_options::PAGE_SCANNER_ENABLE_EVICTION_NAME
            );
        }
        let mut eviction_level = EvictionLevel::IncludeNewest;
        if argc >= 4 && argv[3].as_str() == "only_old" {
            eviction_level = EvictionLevel::OnlyOldest;
        }
        const MB: u64 = 1024 * 1024;
        let bytes = argv[2].arg_uint * MB;
        pmm::evictor().evict_asynchronous(bytes, 0, eviction_level, Output::Print);
    } else if argv[1].as_str() == "pt_reclaim" {
        if argc < 3 {
            return usage();
        }
        let enable = if argv[2].as_str() == "on" {
            true
        } else if argv[2].as_str() == "off" {
            false
        } else {
            return usage();
        };
        if *PAGE_TABLE_RECLAIM_POLICY == PageTableEvictionPolicy::Always {
            kprintln!("Page table reclamation set to always by command line, cannot adjust");
        } else if *PAGE_TABLE_RECLAIM_POLICY == PageTableEvictionPolicy::Never {
            kprintln!("Page table reclamation set to never by command line, cannot adjust");
        } else if enable {
            enable_page_table_reclaim();
        } else {
            disable_page_table_reclaim();
        }
    } else {
        kprintln!("unknown command");
        return usage();
    }
    zx_status::sys::ZX_OK
}

#[cfg(console_enabled)]
static_command!(
    CMD_SCANNER,
    c"scanner".as_ptr(),
    c"active memory scanner".as_ptr(),
    cmd_scanner,
    CMD_AVAIL_ALWAYS
);

// C FFI exports for `scanner.cc` forwarding shims.

/// FFI entry point for [`push_disable_count`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_scanner_push_disable_count() {
    push_disable_count();
}

/// FFI entry point for [`pop_disable_count`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_scanner_pop_disable_count() {
    pop_disable_count();
}

/// FFI entry point for [`debug_dump_state_before_panic`].
///
/// # Safety
///
/// Must only be called immediately prior to a kernel panic when not racing with fundamental state
/// changes or destruction of the threads being inspected, and while not holding any of their
/// thread locks or an active chain lock transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scanner_debug_dump_state_before_panic() {
    // SAFETY: Caller upholds the safety contract of `debug_dump_state_before_panic`.
    unsafe {
        debug_dump_state_before_panic();
    }
}
