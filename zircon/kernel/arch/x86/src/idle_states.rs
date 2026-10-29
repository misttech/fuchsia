// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::CStr;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use x86_idle_states_bindings::X86_CSTATE_C1_MWAIT_HINT;
use zx_types::zx_duration_t;

use super::feature::{
    X86_MAX_CSTATES, x86_get_microarch_config, x86_idle_state_t, x86_idle_states_t,
};

/// Every processor must support at least C1.
const X86_CSTATE_C1_NAME: &CStr = c"C1";

const IDLE_DURATION_FACTOR: zx_duration_t = 3;

/// Returns a base C1 idle state descriptor with the given `exit_latency` (in us).
const fn x86_cstate_c1(exit_latency: u32) -> x86_idle_state_t {
    x86_idle_state_t {
        name: X86_CSTATE_C1_NAME.as_ptr(),
        mwait_hint: X86_CSTATE_C1_MWAIT_HINT,
        exit_latency,
        flushes_tlb: false,
    }
}

const BASE_C1_STATE: x86_idle_state_t = x86_cstate_c1(0);

/// Extracts the 1-based target C-state number (`C1` -> 1, `C2` -> 2, ...) from an `MWAIT` `EAX`
/// hint (`EAX[7:4]` is the 4-bit Target C-state `(C-state - 1)`; sub-states `EAX[3:0]` share the
/// same `state_mask` bit, so `C1E` `0x01` maps to bit 0 alongside base `C1` `0x00`).
#[inline]
const fn state_number_from_mwait_hint(hint: u32) -> u32 {
    ((hint >> 4) & 0x0f) + 1
}

/// Returns whether `state` is the base C1 idle state.
#[inline]
const fn x86_is_base_idle_state(state: &x86_idle_state_t) -> bool {
    state.mwait_hint == X86_CSTATE_C1_MWAIT_HINT
}

/// Returns a read-only pointer to the list of idle states supported by the
/// system.
pub fn x86_get_idle_states() -> &'static x86_idle_states_t {
    &x86_get_microarch_config().expect("x86_microarch_config not initialized").idle_states
}

/// Returns the number of states in `states`.
/// If `states` is invalid (i.e. does not contain [`x86_cstate_c1`]), returns -1.
const fn x86_num_idle_states(states: &x86_idle_states_t) -> i32 {
    let mut num = 0;
    while num < X86_MAX_CSTATES {
        if x86_is_base_idle_state(&states.states[num]) {
            return (num + 1) as i32;
        }
        num += 1;
    }
    // The config must include and end with `x86_cstate_c1`.
    -1
}

/// Per-C-state descriptor and runtime entry/duration statistics.
///
/// `times_entered` and `total_duration` use `Relaxed` atomics for interior mutability: each CPU's
/// idle thread updates its own counters while `k idlestates` may concurrently read or reset
/// counters across CPUs via shared `&'static` references with no cross-variable ordering
/// dependencies.
#[repr(C)]
pub struct X86IdleState {
    // Safety invariant: `state.name` is either null or points to a valid NUL-terminated `'static`
    // C string.
    state: &'static x86_idle_state_t,
    times_entered: AtomicU64,
    total_duration: AtomicI64,
}

impl Default for X86IdleState {
    #[inline]
    fn default() -> Self {
        Self {
            state: &BASE_C1_STATE,
            times_entered: AtomicU64::new(0),
            total_duration: AtomicI64::new(0),
        }
    }
}

zr::static_assert!(core::mem::size_of::<X86IdleState>() == 24);
zr::static_assert!(core::mem::align_of::<X86IdleState>() == 8);

impl X86IdleState {
    /// Creates an `X86IdleState` referencing `state` with zeroed counters.
    ///
    /// # Safety
    ///
    /// `state.name` must be either null or point to a valid NUL-terminated `'static` C string.
    #[inline]
    const unsafe fn new(state: &'static x86_idle_state_t) -> Self {
        Self { state, times_entered: AtomicU64::new(0), total_duration: AtomicI64::new(0) }
    }

    /// Returns the underlying static C-state descriptor.
    #[inline]
    pub(super) const fn raw_state(&self) -> &'static x86_idle_state_t {
        self.state
    }

    /// Returns the human-readable name for the state.
    #[inline]
    pub fn name(&self) -> &str {
        let name_ptr = self.raw_state().name;
        if name_ptr.is_null() {
            core::hint::cold_path();
            return "";
        }
        // SAFETY: By the safety invariant on `self.state`, when `name_ptr` is non-null it points
        // to a valid NUL-terminated `'static` C string.
        unsafe { CStr::from_ptr(name_ptr) }.to_str().unwrap_or("")
    }

    /// Returns the hint to provide to MWAIT to enter this state.
    #[inline]
    pub const fn mwait_hint(&self) -> u32 {
        self.raw_state().mwait_hint
    }

    /// Returns the expected latency (in ns) of exiting the C-state.
    #[inline]
    const fn exit_latency(&self) -> zx_duration_t {
        (self.raw_state().exit_latency as i64) * 1000
    }

    /// Returns whether this state is the base C1 idle state.
    #[inline]
    const fn is_base_state(&self) -> bool {
        x86_is_base_idle_state(self.raw_state())
    }

    /// Returns whether entering the state can result in a TLB flush.
    #[inline]
    const fn flushes_tlb(&self) -> bool {
        self.raw_state().flushes_tlb
    }

    /// Returns the number of times the system entered this state since the last
    /// call to [`Self::reset_counters`].
    #[inline]
    pub fn times_entered(&self) -> u64 {
        // Ordering::Relaxed is sufficient for independent diagnostic statistics counters.
        self.times_entered.load(Ordering::Relaxed)
    }

    /// Returns total time the system has spent in this state since the last
    /// call to [`Self::reset_counters`].
    #[inline]
    pub fn cumulative_duration(&self) -> zx_duration_t {
        // Ordering::Relaxed is sufficient for independent diagnostic duration totals.
        self.total_duration.load(Ordering::Relaxed)
    }

    /// Resets the entry and duration counters.
    #[inline]
    pub fn reset_counters(&self) {
        // Ordering::Relaxed is sufficient for best-effort diagnostic counter resets.
        self.times_entered.store(0, Ordering::Relaxed);
        self.total_duration.store(0, Ordering::Relaxed);
    }

    /// Marks the idle state as entered.
    #[inline]
    pub fn count_entry(&self) {
        // Ordering::Relaxed load+store avoids a locked RMW on the idle exit path; only this CPU's
        // idle thread increments its counters (with best-effort diagnostic reads/resets from
        // `k idlestates`).
        let cur = self.times_entered.load(Ordering::Relaxed);
        self.times_entered.store(cur.wrapping_add(1), Ordering::Relaxed);
    }

    /// Records that the system spent `duration` in this state before exiting.
    #[inline]
    pub fn record_duration(&self, duration: zx_duration_t) {
        // Ordering::Relaxed load+store avoids a locked CAS loop on the idle exit path; only this
        // CPU's idle thread updates its duration (with best-effort diagnostic reads/resets from
        // `k idlestates`).
        let cur = self.total_duration.load(Ordering::Relaxed);
        self.total_duration.store(cur.saturating_add(duration), Ordering::Relaxed);
    }
}

/// Per-CPU table of supported x86 C-states and idle state selection state.
///
/// `last_idle_duration` and `state_mask` use `Relaxed` atomics for interior mutability so each
/// CPU's idle loop and cross-CPU `k idlestates` diagnostic commands operate exclusively through
/// shared `&X86IdleStates` references without aliasing UB or cross-CPU thread migration.
#[repr(C)]
pub struct X86IdleStates {
    states: [X86IdleState; X86_MAX_CSTATES],
    num_states: usize,
    last_idle_duration: AtomicI64,
    state_mask: AtomicU32,
}

zr::static_assert!(core::mem::size_of::<X86IdleStates>() == 312);
zr::static_assert!(core::mem::align_of::<X86IdleStates>() == 8);

impl X86IdleStates {
    /// Creates an `X86IdleStates` table from the static `states` descriptor.
    ///
    /// # Safety
    ///
    /// Every populated entry in `states.states` up to and including the terminating base `C1`
    /// state must have a `name` pointer that is either null or points to a valid NUL-terminated
    /// `'static` C string.
    pub(super) unsafe fn new(states: &'static x86_idle_states_t) -> Self {
        let num_states = x86_num_idle_states(states);
        assert!(
            num_states > 0,
            "Invalid C-state configuration: Expected at least C1 to be defined."
        );
        let num_states = num_states as usize;
        let state_entries = core::array::from_fn(|i| {
            if i < num_states {
                // SAFETY: Caller guarantees populated entries in `states.states` have valid
                // `'static` NUL-terminated `name` C strings.
                unsafe { X86IdleState::new(&states.states[i]) }
            } else {
                // Initialize trailing slots `i >= num_states` to `BASE_C1_STATE` so every element
                // of `[X86IdleState; X86_MAX_CSTATES]` holds a valid `'static` reference.
                X86IdleState::default()
            }
        });
        Self {
            states: state_entries,
            num_states,
            last_idle_duration: AtomicI64::new(0),
            // Always allow C1
            state_mask: AtomicU32::new(states.default_state_mask | 0x1),
        }
    }

    /// Initializes an `X86IdleStates` table in place from the static `states` descriptor.
    ///
    /// # Safety
    ///
    /// - `this` must point to valid, writable, properly aligned storage for `X86IdleStates`.
    /// - Every populated entry in `states.states` up to and including the terminating base `C1`
    ///   state must have a `name` pointer that is either null or points to a valid NUL-terminated
    ///   `'static` C string.
    pub(super) unsafe fn init_ptr(this: *mut Self, states: &'static x86_idle_states_t) {
        // SAFETY: `this` points to valid, writable storage for `Self`, and the caller guarantees
        // `states` upholds `Self::new`'s safety precondition.
        unsafe {
            this.write(Self::new(states));
        }
    }

    /// Returns the list of states supported by the CPU, with the same
    /// ordering constraints as documented for [`x86_idle_states_t`].
    #[inline]
    pub fn states(&self) -> &[X86IdleState] {
        &self.states[..self.num_states]
    }

    /// Returns the number of populated C-states in [`Self::states`].
    #[inline]
    pub fn num_states(&self) -> usize {
        self.num_states
    }

    /// Picks the index into [`Self::states`] of the idle state to enter.
    #[inline]
    pub(super) fn pick_idle_state_index(&self) -> usize {
        let c1_idx = self.num_states.min(X86_MAX_CSTATES).saturating_sub(1);
        // Ordering::Relaxed is sufficient because `last_idle_duration` is only updated by this
        // CPU's idle loop (or single-threaded unit tests).
        let last_idle_duration = self.last_idle_duration.load(Ordering::Relaxed);
        if last_idle_duration <= 0 {
            // Return the shallowest state (C1).
            return c1_idx;
        }
        // Ordering::Relaxed is sufficient because `state_mask` is an independent tuning mask with
        // no cross-variable ordering dependencies.
        let valid_state_mask = self.state_mask.load(Ordering::Relaxed);
        let scaled_duration = IDLE_DURATION_FACTOR.saturating_mul(last_idle_duration);
        // Pick the deepest valid state which has `exit_latency()` less than
        // `IDLE_DURATION_FACTOR` * <expected idle duration>.
        for (idx, state) in self.states[..c1_idx].iter().enumerate() {
            let state_num = state_number_from_mwait_hint(state.mwait_hint());
            if (valid_state_mask & (1u32.checked_shl(state_num.saturating_sub(1)).unwrap_or(0)))
                == 0
            {
                continue;
            }
            if state.exit_latency() < scaled_duration {
                return idx;
            }
        }
        c1_idx
    }

    /// Picks an idle state to enter.
    #[inline]
    pub fn pick_idle_state(&self) -> &X86IdleState {
        &self.states[self.pick_idle_state_index()]
    }

    /// Callback to call when the system becomes idle.
    #[inline]
    pub fn record_duration(&self, duration: zx_duration_t) {
        // Ordering::Relaxed is sufficient because `last_idle_duration` is only read and written
        // by this CPU's idle loop (or single-threaded unit tests).
        self.last_idle_duration.store(duration, Ordering::Relaxed);
    }

    /// Updates the mask of valid C-states.
    #[inline]
    pub fn set_state_mask(&self, mask: u32) {
        // Ordering::Relaxed is sufficient because `state_mask` is an independent tuning mask.
        self.state_mask.store(mask | 0x1, Ordering::Relaxed); // Always allow C1
    }
}

/// Test idle state enumeration and selection (x86 only).
#[cfg(ktest)]
#[unittest::suite(name = "x86_idle_states_rust")]
mod tests {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU8, Ordering};

    use kprint::kprintln;
    use unittest::{assert_eq, expect_eq, expect_false, expect_true};
    use x86_idle_states_bindings::kX86IdleStateMaskC1Only as X86_IDLE_STATE_MASK_C1_ONLY;
    use zx_types::zx_duration_t;

    use super::{
        X86_MAX_CSTATES, X86IdleState, X86IdleStates, x86_cstate_c1, x86_get_idle_states,
        x86_idle_state_t, x86_idle_states_t, x86_num_idle_states,
    };
    use crate::arch_rs::x86::feature::{
        X86_FEATURE_MON, X86CpuidLeafNum, x86_cpu_vulnerable_to_rsb_cross_thread,
        x86_cpu_vulnerable_to_rsb_underflow, x86_feature_test, x86_get_cpuid_leaf,
    };
    use crate::arch_rs::x86::x86::{x86_enable_ints_and_mwait, x86_monitor, x86_ras_fill};
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};

    const EMPTY_STATE: x86_idle_state_t = x86_idle_state_t {
        name: core::ptr::null(),
        mwait_hint: 0,
        exit_latency: 0,
        flushes_tlb: false,
    };

    const C1_ONLY_IDLE_STATES: x86_idle_states_t = {
        let mut states = [EMPTY_STATE; X86_MAX_CSTATES];
        states[0] = x86_cstate_c1(0);
        x86_idle_states_t { states, default_state_mask: X86_IDLE_STATE_MASK_C1_ONLY }
    };

    const KABY_LAKE_IDLE_STATES: x86_idle_states_t = {
        let mut states = [EMPTY_STATE; X86_MAX_CSTATES];
        states[0] = x86_idle_state_t {
            name: c"C6".as_ptr(),
            mwait_hint: 0x50,
            exit_latency: 1000,
            flushes_tlb: true,
        };
        states[1] = x86_idle_state_t {
            name: c"C3".as_ptr(),
            mwait_hint: 0x20,
            exit_latency: 100,
            flushes_tlb: true,
        };
        states[2] = x86_idle_state_t {
            name: c"C1E".as_ptr(),
            mwait_hint: 0x01,
            exit_latency: 10,
            flushes_tlb: false,
        };
        states[3] = x86_cstate_c1(0);
        x86_idle_states_t { states, default_state_mask: 0b0000_0000_0011_1111 }
    };

    /// Select an idle state using data from a CPU with only C1.
    #[test]
    fn test_c1_only() {
        // SAFETY: `C1_ONLY_IDLE_STATES` contains valid NUL-terminated `'static` C strings.
        let states = unsafe { X86IdleStates::new(&C1_ONLY_IDLE_STATES) };
        assert_eq!(states.num_states(), 1);
        let state = states.pick_idle_state();
        expect_true!(state.name() == "C1");
        expect_eq!(state.mwait_hint(), 0x00);
        expect_true!(state.is_base_state());
    }

    /// Select an idle state using data from a Kabylake CPU.
    #[test]
    fn test_kbl() {
        // SAFETY: `KABY_LAKE_IDLE_STATES` contains valid NUL-terminated `'static` C strings.
        let states = unsafe { X86IdleStates::new(&KABY_LAKE_IDLE_STATES) };
        assert_eq!(states.num_states(), 4);

        let mut state = states.pick_idle_state();
        expect_true!(state.name() == "C1");
        expect_eq!(state.mwait_hint(), 0x00);

        states.record_duration(DurationMono::from_micros(3).0);
        state = states.pick_idle_state();
        expect_true!(state.name() == "C1");
        expect_eq!(state.mwait_hint(), 0x00);

        states.record_duration(DurationMono::from_micros(4).0);
        state = states.pick_idle_state();
        expect_true!(state.name() == "C1E");
        expect_eq!(state.mwait_hint(), 0x01);

        states.record_duration(DurationMono::from_micros(34).0);
        state = states.pick_idle_state();
        expect_true!(state.name() == "C3");
        expect_eq!(state.mwait_hint(), 0x20);

        states.record_duration(DurationMono::from_micros(334).0);
        state = states.pick_idle_state();
        expect_true!(state.name() == "C6");
        expect_eq!(state.mwait_hint(), 0x50);
    }

    /// Select an idle state using data from a Kabylake CPU, respecting a mask of valid states.
    #[test]
    fn test_kbl_statemask() {
        // SAFETY: `KABY_LAKE_IDLE_STATES` contains valid NUL-terminated `'static` C strings.
        let states = unsafe { X86IdleStates::new(&KABY_LAKE_IDLE_STATES) };

        // Empty mask will always choose C1 or C1E
        states.set_state_mask(0b0000_0000_0000_0000);
        let mut state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x00);
        expect_true!(state.name() == "C1");
        states.record_duration(DurationMono::from_micros(3).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x00);
        expect_true!(state.name() == "C1");
        states.record_duration(DurationMono::from_micros(4).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x01);
        expect_true!(state.name() == "C1E");
        states.record_duration(DurationMono::from_micros(34).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x01);
        expect_true!(state.name() == "C1E");
        states.record_duration(DurationMono::from_micros(334).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x01);
        expect_true!(state.name() == "C1E");

        // Mask to only allow C6, C1/C1E
        states.set_state_mask(0b0000_0000_0010_0001);
        states.record_duration(DurationMono::from_micros(0).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x00);
        expect_true!(state.name() == "C1");
        states.record_duration(DurationMono::from_micros(3).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x00);
        expect_true!(state.name() == "C1");
        states.record_duration(DurationMono::from_micros(4).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x01);
        expect_true!(state.name() == "C1E");
        states.record_duration(DurationMono::from_micros(34).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x01);
        expect_true!(state.name() == "C1E");
        states.record_duration(DurationMono::from_micros(334).0);
        state = states.pick_idle_state();
        expect_eq!(state.mwait_hint(), 0x50);
        expect_true!(state.name() == "C6");
    }

    const NO_C1_IDLE_STATES: x86_idle_states_t = {
        let non_c1 = x86_idle_state_t {
            name: c"C3".as_ptr(),
            mwait_hint: 0x20,
            exit_latency: 100,
            flushes_tlb: true,
        };
        x86_idle_states_t {
            states: [non_c1; X86_MAX_CSTATES],
            default_state_mask: X86_IDLE_STATE_MASK_C1_ONLY,
        }
    };

    const NULL_NAME_DESCRIPTOR: x86_idle_state_t = x86_idle_state_t {
        name: core::ptr::null(),
        mwait_hint: 0x20,
        exit_latency: 42,
        flushes_tlb: true,
    };

    /// Verify C-state validation, metadata accessors, and saturating counter accounting.
    #[test]
    fn test_state_metadata_and_counters() {
        expect_eq!(x86_num_idle_states(&NO_C1_IDLE_STATES), -1);

        // SAFETY: `NULL_NAME_DESCRIPTOR.name` is null, which `X86IdleState::new` permits.
        let state = unsafe { X86IdleState::new(&NULL_NAME_DESCRIPTOR) };
        expect_true!(state.name().is_empty());
        expect_eq!(state.exit_latency(), 42_000);
        expect_true!(state.flushes_tlb());
        expect_false!(state.is_base_state());
        expect_eq!(state.times_entered(), 0);
        expect_eq!(state.cumulative_duration(), 0);

        state.count_entry();
        state.count_entry();
        state.record_duration(zx_duration_t::MAX - 10);
        expect_eq!(state.times_entered(), 2);
        expect_eq!(state.cumulative_duration(), zx_duration_t::MAX - 10);

        // `record_duration` saturates at `zx_duration_t::MAX` rather than overflowing.
        state.record_duration(100);
        expect_eq!(state.cumulative_duration(), zx_duration_t::MAX);

        state.reset_counters();
        expect_eq!(state.times_entered(), 0);
        expect_eq!(state.cumulative_duration(), 0);
    }

    #[repr(C, align(64))]
    struct AlignedMonitor(AtomicU8, [u8; 63]);

    static MONITOR: AlignedMonitor = AlignedMonitor(AtomicU8::new(0), [0; 63]);

    const GUARD_VALUE: u8 = u8::MAX;

    extern "C" fn poke_monitor(_arg: *mut c_void) -> i32 {
        // A short sleep ensures the main test thread has time to set up the monitor
        // and enter MWAIT.
        let _ = crate::kernel::thread::sleep_relative(DurationMono::from_millis(1));
        // Hardware `MWAIT` monitors cache-line invalidation; `Ordering::Release` also pairs with
        // the `Acquire` re-check before `MWAIT` so stores completing before `MONITOR` arms are
        // observed even without a subsequent cache-line invalidation.
        MONITOR.0.store(GUARD_VALUE, Ordering::Release);
        0
    }

    /// Enter each supported idle state using MWAIT/MONITOR.
    #[test]
    fn test_enter_idle_states() {
        // Ordering::Relaxed is sufficient because hardware `MWAIT` monitors cache-line
        // invalidation rather than relying on software memory ordering.
        MONITOR.0.store(0, Ordering::Relaxed);

        if x86_feature_test(X86_FEATURE_MON) {
            let mwait_extensions_supported =
                x86_get_cpuid_leaf(X86CpuidLeafNum::Mon).is_some_and(|leaf| (leaf.c & 0x1) != 0);
            // SAFETY: `x86_get_idle_states()` returns a static table with valid NUL-terminated
            // `'static` C strings.
            let states = unsafe { X86IdleStates::new(x86_get_idle_states()) };
            for (i, state) in states.states().iter().enumerate() {
                let mwait_hint = if mwait_extensions_supported { state.mwait_hint() } else { 0 };
                kprintln!(
                    "Entering state '{:s}' (MWAIT 0x{:02x}) on CPU {}",
                    state.name(),
                    mwait_hint,
                    crate::arch_rs::curr_cpu_num()
                );

                // Hardware `MWAIT` monitors cache-line invalidation; initialize `MONITOR` with
                // `Ordering::Release` before starting `poke_monitor` so a preempted test thread
                // cannot overwrite `GUARD_VALUE` after `poke_monitor` runs.
                MONITOR.0.store(i as u8, Ordering::Release);

                // Thread must be created and started before arming the monitor,
                // since thread creation appears to trip the monitor latch prematurely.
                // SAFETY: `poke_monitor` is a valid thread entry function that takes no argument.
                let thrd = unsafe {
                    crate::kernel::thread::create_with_priority(
                        c"monitor_poker".as_ptr(),
                        poke_monitor,
                        core::ptr::null_mut(),
                        crate::kernel::thread::DEFAULT_PRIORITY,
                    )
                }
                .unwrap();
                // SAFETY: `thrd` was just created and has not been resumed or joined yet.
                unsafe { thrd.resume() };

                let start = current_mono_time();
                {
                    let _apd = crate::kernel::thread::AutoPreemptDisabler::new();
                    let int_state = crate::arch_rs::arch_interrupt_save();
                    // SAFETY: `CPL == 0`, `X86_FEATURE_MON` is supported, and `MONITOR` resides in
                    // 64-byte-aligned Write-Back kernel `.bss` memory.
                    unsafe { x86_monitor(MONITOR.0.as_ptr().cast::<c_void>()) };
                    // Re-read `MONITOR` after arming `x86_monitor` (with interrupts disabled) to
                    // avoid a lost wakeup if `poke_monitor` stored `GUARD_VALUE` before `MONITOR`.
                    if MONITOR.0.load(Ordering::Acquire) != GUARD_VALUE {
                        if x86_cpu_vulnerable_to_rsb_cross_thread() {
                            x86_ras_fill();
                        }
                        // SAFETY: `CPL == 0`, `X86_FEATURE_MON` is supported, interrupts were
                        // disabled by `arch_interrupt_save`, no spinlocks or interrupt-disable
                        // guards are held, and RSB mitigations are applied around deep C-states.
                        unsafe { x86_enable_ints_and_mwait(mwait_hint) };
                        crate::arch_rs::disable_ints();
                        if x86_cpu_vulnerable_to_rsb_underflow() && mwait_hint >= 0x20 {
                            x86_ras_fill();
                        }
                    }
                    crate::arch_rs::arch_interrupt_restore(int_state);
                }

                kprintln!("Exiting state ({} ns elapsed)", (current_mono_time() - start).0);
                // SAFETY: `thrd` was resumed above and has not been joined yet.
                let _ = unsafe { thrd.join(InstantMono::INFINITE) };
            }
        } else {
            kprintln!("Skipping test; MWAIT/MONITOR not supported");
        }
    }
}
