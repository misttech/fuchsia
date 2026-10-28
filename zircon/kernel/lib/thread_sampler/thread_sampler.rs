// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Periodic thread stack sampler and FXT profiler record emitter.

mod thread_sampler_ffi;

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::{without_provenance, without_provenance_mut};
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use fbl::{Array, Vector};
use fxt_layout::ProfilerRecordType;
use kprint::kprint;
use ksync::LockToken;
use zerocopy::IntoBytes;
use zx_status::Status;
use zx_types::{zx_koid_t, zx_sampler_config_t};

use crate::arch_rs::{GeneralRegsSource, InterruptDisableGuard, curr_cpu_num, ints_disabled};
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::mp::{MpIpiTarget, sync_exec};
use crate::kernel::percpu::PerCpu;
use crate::kernel::thread::{cpp_thread_signal_sample_stack, sleep_relative};
use crate::kernel::types::cpu_num_t;
use crate::ktrace_rs::{RecordHeader, RecordType};
use crate::percpu_writer::Buffer;
use crate::platform_rs::timer::{
    DurationMono, current_mono_time, timer_current_mono_ticks, zx_msec, zx_sec,
};
use crate::user_copy::{UserInPtr, UserOutPtr};

ksync::declare_singleton_mutex!(ThreadSamplerLock);

/// The current state of the thread sampler.
///
/// Valid state transitions are:
///
/// ```text
///    /- [ Destroying ] <--- \
///    |                      |
///    v                      |
/// [ Unallocated ] -> [ Configured ] -> [ Running ]
///                             ^          |
///                             |          v
///                             \-[ Stopping ]
/// ```
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SamplingState {
    /// The idle state for the sampler. We're not actively sampling nor is there a user handle
    /// associated with the thread sampler singleton.
    Unallocated = 0,

    /// We have buffers allocated and the user has a handle to us and can start a session. Reading
    /// is allowed, but not writing.
    Configured = 1,

    /// The session is in progress. We are taking samples and writing data. Both reading and
    /// writing are allowed.
    Running = 2,

    /// The session is stopping, no more write references to the buffers are allowed to be created
    /// and we're waiting for existing write references to be released.
    Stopping = 3,

    /// We have stopped giving out read references and are waiting for all existing read references
    /// to close. Once there are no buffer references, we'll destroy the buffers and become
    /// unallocated.
    Destroying = 4,
}

impl SamplingState {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Unallocated,
            1 => Self::Configured,
            2 => Self::Running,
            3 => Self::Stopping,
            4 => Self::Destroying,
            _ => Self::Unallocated,
        }
    }
}

/// RAII reference to a single CPU's [`Buffer`] for writing a sample.
///
/// Holds both a buffer reference count and a write reference count in `ThreadSampler::state`,
/// preventing the buffers from being destroyed or the session from completing `Stop` until
/// dropped.
struct PerCpuBufferRef<'a> {
    buffer: &'a Buffer,
    state: &'a AtomicU64,
}

impl<'a> PerCpuBufferRef<'a> {
    /// Returns a shared reference to the referenced per-CPU [`Buffer`].
    #[inline]
    const fn get(&self) -> &'a Buffer {
        self.buffer
    }
}

impl Drop for PerCpuBufferRef<'_> {
    fn drop(&mut self) {
        const DECREMENT: u64 =
            ThreadSampler::BUFFER_REF_COUNT_INCREMENT + ThreadSampler::WRITE_REF_COUNT_INCREMENT;
        self.state.fetch_sub(DECREMENT, Ordering::AcqRel);
    }
}

/// RAII reference to all per-CPU [`Buffer`]s for reading trace data.
///
/// Holds a buffer reference count in `ThreadSampler::state`, preventing the buffers from being
/// destroyed until dropped.
struct ReadToken<'a> {
    buffers: &'a [Buffer],
    state: &'a AtomicU64,
}

impl<'a> ReadToken<'a> {
    /// Returns the slice of per-CPU [`Buffer`]s.
    #[inline]
    const fn get(&self) -> &'a [Buffer] {
        self.buffers
    }
}

impl Drop for ReadToken<'_> {
    fn drop(&mut self) {
        self.state.fetch_sub(ThreadSampler::BUFFER_REF_COUNT_INCREMENT, Ordering::AcqRel);
    }
}

/// RAII guard ensuring at most one concurrent consumer in [`ThreadSampler::read_user`].
struct ReadConsumerGuard<'a> {
    reading: &'a AtomicBool,
}

impl<'a> ReadConsumerGuard<'a> {
    fn try_acquire(reading: &'a AtomicBool) -> Option<Self> {
        reading
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self { reading })
    }
}

impl Drop for ReadConsumerGuard<'_> {
    fn drop(&mut self) {
        self.reading.store(false, Ordering::Release);
    }
}

/// Global periodic thread stack sampler.
///
/// We have only a single global thread sampler at a time. Other callers to [`Self::set_up`] will
/// receive [`Status::ALREADY_EXISTS`] until the existing sampler is released via
/// [`Self::destroy`].
pub struct ThreadSampler {
    /// `per_cpu_buffers` and the `SamplingState` bits of `state` may be READ without acquiring
    /// `ThreadSamplerLock`. However, the lock must be acquired to WRITE them.
    ///
    /// `per_cpu_buffers` must not be modified while the session is in the states:
    ///  - `Configured`
    ///  - `Running`
    ///  - `Stopping`
    ///
    /// `state` is eight bytes composed as:
    ///
    ///   `MM MM WW WW BB BB XX SS`
    ///
    ///   - `SS`: 8 bits, `SamplingState`
    ///   - `XX`: 8 bits, Reserved
    ///   - `BB`: 16 bits, `BufferRefCount`
    ///   - `WW`: 16 bits, `WriteRefCount`
    ///   - `MM`: 16 bits, `MarkingsScheduled`
    ///
    /// Rules for our state:
    ///
    /// `BufferRefCount` is used to control when we can transition from `Configured` to
    /// `Unallocated`. `BufferRefCount` is incremented when a caller reads data through
    /// `zx_sampler_read`.
    /// - `BufferRefCount` must be zero if `State` is `Unallocated`.
    /// - `BufferRefCount` can only be incremented in states `Configured`, `Stopping`, and
    ///   `Running`.
    /// - We use the intermediate state `Destroying` to prevent new references and wait for
    ///   existing references to close.
    /// - We cannot transition to `Unallocated` until `BufferRefCount` is 0.
    ///
    /// `WriteRefCount` is used to control when we can transition from `Running` to `Configured`.
    /// `WriteRefCount` is incremented before a writer attempts to access any per-cpu buffer, and
    /// decremented when it's done writing.
    /// - `WriteRefCount` must be zero if `State` is `Unallocated`, `Configured`, or `Destroying`.
    /// - `WriteRefCount` can only be incremented in the state `Running`.
    /// - We use the intermediate state `Stopping` to prevent new Write references and wait for
    ///   existing ones to close.
    /// - We cannot transition from `Running` to `Configured` until `WriteRefCount` is 0.
    /// - As we always require a `BufferRef` in order to write, it will always be that
    ///   `BufferRef >= WriteRefCount`.
    ///
    /// `MarkingsScheduled` is also used to control when we can transition from `Running` to
    /// `Configured`. `MarkingsScheduled` is equal to the number of timer callbacks running or
    /// scheduled to run. It is decremented when we cancel the timer callbacks or a timer callback
    /// finds that the state is no longer `Running`.
    /// - `MarkingsScheduled` must be zero if `State` is `Unallocated`, `Configured`, or
    ///   `Destroying`.
    /// - `MarkingsScheduled` can only be incremented in the state `Running`.
    /// - We use the intermediate state `Stopping` to prevent new timers and wait for existing ones
    ///   to close.
    /// - We cannot transition from `Running` to `Configured` until `MarkingsScheduled` is 0.
    ///
    /// Memory ordering synchronization:
    /// - State transitions in `set_state` and refcount increments/decrements in
    ///   `get_buffer_ref_for_writing`, `get_buffer_ref_for_reading`, `PerCpuBufferRef::drop`,
    ///   `ReadToken::drop`, `schedule_marking`, and `reschedule_marking` use `Ordering::AcqRel`
    ///   (and `Ordering::SeqCst` in `cancel_marking`).
    /// - Wait loops in `stop_locked` and `destroy` load `state` with `Ordering::Acquire`,
    ///   synchronizing with the `Release` half of the final refcount decrement before mutating
    ///   `per_cpu_buffers`.
    state: AtomicU64,

    /// The current session. Monotonically increasing.
    ///
    /// Used to prevent scheduled markings from being so delayed that they trigger in a subsequent
    /// session by marking threads with which `session_id` their next sample is intended for.
    session_id: AtomicU64,

    /// Sampling period in nanoseconds. Written under `ThreadSamplerLock` in `set_up` before
    /// publishing `SamplingState::Configured` / `Running` via `set_state` (`Ordering::AcqRel`).
    sample_period: AtomicI64,

    /// Serializes consumer `SpscBuffer::read` calls in `read_user` across concurrent callers.
    reading: AtomicBool,

    // Safety invariant: Mutating the `Option<Array<Buffer>>` (setting `Some` in `set_up` or
    // resetting to `None` in `destroy`) requires holding `ThreadSamplerLock` while `state` is
    // `Unallocated` and `BufferRefCount == 0`. Shared references `&[Buffer]` are only created
    // while `BufferRefCount > 0` in states `{Configured, Running, Stopping}`, or while
    // `ThreadSamplerLock` is held and no concurrent mutation is occurring. Each per-CPU `Buffer`
    // uses internal atomic SPSC pointers and `UnsafeCell<DroppedRecordStats>` guarded by
    // `&InterruptDisableGuard` on the owning CPU.
    per_cpu_buffers: UnsafeCell<Option<Array<Buffer>>>,
}

// SAFETY: Concurrent access to `ThreadSampler` is synchronized by `ThreadSamplerLock` and the
// atomic `state` refcount protocol documented on `state` and `per_cpu_buffers`.
unsafe impl Sync for ThreadSampler {}

/// The single global [`ThreadSampler`] instance.
pub static THREAD_SAMPLER: ThreadSampler = ThreadSampler::new();

/// Joins the sampling session on the current CPU if one exists (`sampler::sampler_percpu_init`).
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
pub unsafe fn sampler_percpu_init() {
    // SAFETY: The caller guarantees interrupts are disabled on the current CPU.
    unsafe { THREAD_SAMPLER.schedule_marking() };
}

/// Exits the sampling session on the current CPU if one exists
/// (`sampler::sampler_percpu_shutdown`).
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
pub unsafe fn sampler_percpu_shutdown() {
    // SAFETY: The caller guarantees interrupts are disabled on the current CPU.
    unsafe { THREAD_SAMPLER.cancel_marking() };
}

/// Writes an FXT profiler backtrace record (`fxt::WriteProfilerBacktraceRecord`) to `writer`.
///
/// # Safety
///
/// - `writer` must be the current CPU's per-CPU buffer.
/// - No uncommitted [`Reservation`](crate::percpu_writer::Reservation) for `writer` may be live
///   when this function is called.
unsafe fn write_profiler_backtrace_record(
    writer: &Buffer,
    irq_guard: &InterruptDisableGuard,
    timestamp: u64,
    pid: u64,
    tid: u64,
    bt: &[usize],
) -> Result<(), Status> {
    let truncated_len = bt.len().min(0xFF);
    // 1 header word + 1 timestamp word + 2 inline thread_ref words (pid + tid) + N frame words.
    let mut hdr = RecordHeader::new();
    hdr.set_record_type(RecordType::Profiler).set_record_size((4 + truncated_len) as u16);
    let header = hdr.bits()
        | ((ProfilerRecordType::Backtrace as u64) << 16)
        | ((truncated_len as u64) << 28);
    // SAFETY: The caller guarantees `writer` is owned by the current CPU with no live
    // `Reservation`.
    let mut res = unsafe { writer.reserve(irq_guard, header) }?;
    res.write_bytes([timestamp, pid, tid].as_bytes());
    res.write_bytes(bt[..truncated_len].as_bytes());
    res.commit();
    Ok(())
}

impl ThreadSampler {
    const STATE_MASK_SHIFT: u32 = 0;
    const STATE_MASK: u64 = 0xFF << Self::STATE_MASK_SHIFT;

    const BUFFER_REF_COUNT_SHIFT: u32 = 16;
    const BUFFER_REF_COUNT_INCREMENT: u64 = 1u64 << Self::BUFFER_REF_COUNT_SHIFT;
    const BUFFER_REF_COUNT_MASK: u64 = 0xFFFFu64 << Self::BUFFER_REF_COUNT_SHIFT;

    const WRITE_REF_COUNT_SHIFT: u32 = 32;
    const WRITE_REF_COUNT_INCREMENT: u64 = 1u64 << Self::WRITE_REF_COUNT_SHIFT;
    const WRITE_REF_COUNT_MASK: u64 = 0xFFFFu64 << Self::WRITE_REF_COUNT_SHIFT;

    const MARKINGS_SCHEDULED_SHIFT: u32 = 48;
    const MARKINGS_SCHEDULED_INCREMENT: u64 = 1u64 << Self::MARKINGS_SCHEDULED_SHIFT;
    const MARKINGS_SCHEDULED_MASK: u64 = 0xFFFFu64 << Self::MARKINGS_SCHEDULED_SHIFT;

    /// Creates a new unallocated `ThreadSampler`.
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            session_id: AtomicU64::new(0),
            sample_period: AtomicI64::new(0),
            reading: AtomicBool::new(false),
            per_cpu_buffers: UnsafeCell::new(None),
        }
    }

    /// Returns the current [`SamplingState`].
    #[inline]
    fn state(&self) -> SamplingState {
        SamplingState::from_raw((self.state.load(Ordering::Acquire) & Self::STATE_MASK) as u8)
    }

    fn set_state(&self, new_state: SamplingState, _token: &LockToken<'_, ThreadSamplerLock>) {
        // While the `SamplingState` component of `state` won't change out from under us as we
        // require `ThreadSamplerLock` to change it, the writes-in-flight counter could change, so
        // we use a cmpxchg loop to avoid losing a buffer ref count increment or decrement.
        let mut expected = self.state.load(Ordering::Relaxed);
        loop {
            let desired = (expected & !Self::STATE_MASK) | (new_state as u64);
            match self.state.compare_exchange_weak(
                expected,
                desired,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => expected = actual,
            }
        }
    }

    /// Atomically acquires a write reference to the buffer for `cpu_num` while in
    /// [`SamplingState::Running`], ensuring the buffers are not destroyed until the returned
    /// [`PerCpuBufferRef`] is dropped.
    fn get_buffer_ref_for_writing(&self, cpu_num: cpu_num_t) -> Option<PerCpuBufferRef<'_>> {
        if (cpu_num as usize) >= PerCpu::processor_count() {
            return None;
        }
        const INCREMENT: u64 =
            ThreadSampler::BUFFER_REF_COUNT_INCREMENT + ThreadSampler::WRITE_REF_COUNT_INCREMENT;
        let mut expected = self.state.load(Ordering::Relaxed);
        loop {
            let state = SamplingState::from_raw((expected & Self::STATE_MASK) as u8);
            if state != SamplingState::Running {
                return None;
            }
            if ((expected & Self::BUFFER_REF_COUNT_MASK) == Self::BUFFER_REF_COUNT_MASK)
                || ((expected & Self::WRITE_REF_COUNT_MASK) == Self::WRITE_REF_COUNT_MASK)
            {
                // This shouldn't happen, but we should handle it in release builds regardless.
                debug_assert!(
                    (expected & Self::BUFFER_REF_COUNT_MASK) != Self::BUFFER_REF_COUNT_MASK
                );
                debug_assert!(
                    (expected & Self::WRITE_REF_COUNT_MASK) != Self::WRITE_REF_COUNT_MASK
                );
                return None;
            }
            let desired = expected.wrapping_add(INCREMENT);
            match self.state.compare_exchange_weak(
                expected,
                desired,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => expected = actual,
            }
        }

        // SAFETY: We verified `state == SamplingState::Running` and atomically incremented
        // `BufferRefCount` and `WriteRefCount` with `Ordering::AcqRel` above, so `per_cpu_buffers`
        // is guaranteed to be `Some` with length `PerCpu::processor_count()` and immutable until
        // `INCREMENT` is subtracted.
        let buffers = unsafe { (*self.per_cpu_buffers.get()).as_deref() };
        if let Some(buffer) = buffers.and_then(|b| b.get(cpu_num as usize)) {
            Some(PerCpuBufferRef { buffer, state: &self.state })
        } else {
            self.state.fetch_sub(INCREMENT, Ordering::AcqRel);
            None
        }
    }

    /// Atomically acquires a read reference to the per-CPU buffers while allocated, ensuring the
    /// buffers are not destroyed until the returned [`ReadToken`] is dropped.
    fn get_buffer_ref_for_reading(&self) -> Option<ReadToken<'_>> {
        let mut expected = self.state.load(Ordering::Relaxed);
        loop {
            let state = SamplingState::from_raw((expected & Self::STATE_MASK) as u8);
            if state == SamplingState::Unallocated || state == SamplingState::Destroying {
                return None;
            }
            if (expected & Self::BUFFER_REF_COUNT_MASK) == Self::BUFFER_REF_COUNT_MASK {
                // This shouldn't happen, but we should handle it in release builds regardless.
                debug_assert!(
                    (expected & Self::BUFFER_REF_COUNT_MASK) != Self::BUFFER_REF_COUNT_MASK
                );
                return None;
            }
            let desired = expected.wrapping_add(Self::BUFFER_REF_COUNT_INCREMENT);
            match self.state.compare_exchange_weak(
                expected,
                desired,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => expected = actual,
            }
        }

        // SAFETY: We verified `state` is in `{Configured, Running, Stopping}` and incremented
        // `BufferRefCount` with `Ordering::AcqRel`, so `per_cpu_buffers` is guaranteed to be
        // `Some` and immutable until the returned `ReadToken` is dropped.
        let Some(buffers) = (unsafe { (*self.per_cpu_buffers.get()).as_deref() }) else {
            self.state.fetch_sub(Self::BUFFER_REF_COUNT_INCREMENT, Ordering::AcqRel);
            return None;
        };
        Some(ReadToken { buffers, state: &self.state })
    }

    /// Allocates and configures per-CPU buffers for a new sampling session.
    pub fn set_up(&self, config: &zx_sampler_config_t) -> Result<(), Status> {
        let mut per_cpu_buffers: Option<Array<Buffer>> = None;
        ksync::lock!(let mut guard = ThreadSamplerLock::lock());
        if self.state() != SamplingState::Unallocated {
            return Err(Status::ALREADY_EXISTS);
        }
        if config.period <= 0 {
            return Err(Status::INVALID_ARGS);
        }

        let num_cpus = PerCpu::processor_count();
        // SAFETY: `ThreadSamplerLock` is held and `state == Unallocated` (`BufferRefCount == 0`).
        let is_empty = unsafe { (*self.per_cpu_buffers.get()).is_none() };
        if is_empty {
            // Perform the allocations for the state without the lock held as this may potentially
            // block waiting for memory.
            let alloc_res: Result<Array<Buffer>, Status> = guard.as_mut().call_unlocked(|| {
                let mut buffers = Vector::new();
                buffers.reserve(num_cpus).map_err(|_| Status::NO_MEMORY)?;

                // Even though the buffer is per-CPU, we are fine to set up each CPU state here on
                // a single CPU. When we start sampling, we call `sync_exec` which will synchronize
                // the written `per_cpu_buffers`.
                let Ok(buffer_size) = u32::try_from(config.buffer_size) else {
                    return Err(Status::INVALID_ARGS);
                };
                for i in 0..num_cpus {
                    let buf = Buffer::new(buffer_size, b"sampler", 0, i as u64)?;
                    buffers.push_back(buf).map_err(|_| Status::NO_MEMORY)?;
                }
                buffers.try_into_array().map_err(|_| Status::NO_MEMORY)
            });
            per_cpu_buffers = Some(alloc_res?);

            // Reload and check state again as it may have changed while the lock was dropped.
            if self.state() != SamplingState::Unallocated {
                return Err(Status::ALREADY_EXISTS);
            }
        }

        // Re-check whether `per_cpu_buffers` exists as this may have changed while the lock was
        // dropped. If we raced and someone else allocated state before us this is fine and we will
        // just drop the local allocation.
        // SAFETY: `ThreadSamplerLock` is held and `state == Unallocated` (`BufferRefCount == 0`).
        let slot = unsafe { &mut *self.per_cpu_buffers.get() };
        if slot.is_some() {
            return Err(Status::ALREADY_EXISTS);
        }

        assert!(per_cpu_buffers.is_some());
        *slot = per_cpu_buffers;
        self.sample_period.store(config.period, Ordering::Relaxed);
        self.set_state(SamplingState::Configured, guard.token());
        Ok(())
    }

    /// Starts the sampling session across all online CPUs.
    pub fn start(&self) -> Result<(), Status> {
        ksync::lock!(let guard = ThreadSamplerLock::lock());
        if self.state() != SamplingState::Configured {
            return Err(Status::BAD_STATE);
        }
        // SAFETY: `ThreadSamplerLock` is held and `state == Configured`.
        debug_assert!(unsafe {
            (*self.per_cpu_buffers.get()).as_ref().is_some_and(|b| !b.is_empty())
        });

        self.set_state(SamplingState::Running, guard.token());
        sync_exec(MpIpiTarget::All, 0, || {
            // SAFETY: `sync_exec` callbacks execute with interrupts disabled on the target CPU.
            unsafe { self.schedule_marking() };
        });
        Ok(())
    }

    /// Stops the active sampling session and waits for all in-flight writes and timer callbacks
    /// to complete.
    pub fn stop(&self) -> Result<(), Status> {
        ksync::lock!(let guard = ThreadSamplerLock::lock());
        if self.state() != SamplingState::Running {
            return Err(Status::BAD_STATE);
        }
        self.stop_locked(guard.token());
        Ok(())
    }

    fn stop_locked(&self, token: &LockToken<'_, ThreadSamplerLock>) {
        // We start by disabling new writes, timers, and buffer write references. Then we need to
        // wait for any that are in flight to finish.
        self.set_state(SamplingState::Stopping, token);

        self.session_id.fetch_add(1, Ordering::Relaxed);
        // We could be racing with the timer callback here. If the timer callback is currently
        // executing, we could end up in a state where the timer is canceled, but we don't reset
        // the `MarkingsScheduled` bits. As the timer callback occurs with interrupts disabled, we
        // cancel the timers on the CPUs they trigger to serialize the cancellation with a
        // potential callback.
        sync_exec(MpIpiTarget::All, 0, || {
            // SAFETY: `sync_exec` callbacks execute with interrupts disabled on the target CPU.
            unsafe { self.cancel_marking() };
        });

        // Some timers may not have been able to be canceled, so we need to wait for any samples
        // that have already started to finish.
        let mut sleep_duration: i64 = zx_msec(1);
        const WARN_DURATION: i64 = zx_sec(30);
        const MAX_SLEEP_DURATION: i64 = zx_sec(1);
        let mut next_warn_time = current_mono_time().0.saturating_add(WARN_DURATION);
        let mut warn_events: i64 = 0;

        const PENDING_OPERATIONS_MASK: u64 =
            ThreadSampler::WRITE_REF_COUNT_MASK | ThreadSampler::MARKINGS_SCHEDULED_MASK;
        // Stopping ends the session, but keeps the buffers alive so reads may exfiltrate any
        // remaining data. Here, we need to wait for all writes and markings to finish.
        while (self.state.load(Ordering::Acquire) & PENDING_OPERATIONS_MASK) != 0 {
            // Warn if we have spent an 'unreasonable' amount of time waiting.
            if current_mono_time().0 > next_warn_time {
                warn_events += 1;
                kprint!(
                    "WARNING: Waited more than {} seconds for sampling to finish\n",
                    (warn_events * WARN_DURATION) / zx_sec(1)
                );
                next_warn_time = next_warn_time.saturating_add(WARN_DURATION);
            }
            let _ = sleep_relative(DurationMono(sleep_duration));
            // Scale up the sleep duration to balance being initially responsive and not consuming
            // excessive CPU.
            sleep_duration = (sleep_duration * 2).min(MAX_SLEEP_DURATION);
        }

        // At this point, there are no longer pending writes or marks. There may still be threads
        // that have been marked to be sampled, but have not yet attempted a sample. In addition,
        // there may still be buffer read references.
        //
        // As the sampler has static lifetime, these threads can safely access `state` and will see
        // that the session is no longer running, aborting their attempt at a sample.
        self.set_state(SamplingState::Configured, token);
    }

    /// Stops the session (if running), waits for all read references to close, and destroys the
    /// per-CPU buffers.
    pub fn destroy(&self) -> Result<(), Status> {
        ksync::lock!(let guard = ThreadSamplerLock::lock());
        let token = guard.token();
        if self.state() == SamplingState::Running {
            // The userspace end of the sampler has closed and we've skipped right to destroying.
            self.stop_locked(token);
        }
        self.set_state(SamplingState::Destroying, token);

        // Some timers may not have been able to be canceled, so we need to wait for any samples
        // that have already started to finish.
        //
        // In order to destroy the buffers, we should have no references and no in-flight samples.
        // After `stop_locked`, the `WriteRefCount` and `MarkingsScheduled` must be 0, and can no
        // longer be incremented. So we only now need to wait for all buffer references to close.
        let deadline = current_mono_time().0.saturating_add(zx_sec(30));
        const PENDING_OPERATIONS_MASK: u64 = ThreadSampler::BUFFER_REF_COUNT_MASK;
        let mut pending_operations;
        loop {
            pending_operations = self.state.load(Ordering::Acquire) & PENDING_OPERATIONS_MASK;
            if pending_operations != 0 {
                let _ = sleep_relative(DurationMono(zx_msec(1)));
            }
            if pending_operations == 0 || current_mono_time().0 >= deadline {
                break;
            }
        }
        // We'll wait an unreasonable amount of time for the operations to finish. If the
        // operations really haven't finished by this point, something has gone terribly wrong.
        if pending_operations != 0 {
            kprint!("WARNING: Timed out after waiting 30 seconds for sampler destruction\n");
            return Err(Status::BAD_STATE);
        }

        // After `stop_locked`, we have prevented further threads from accessing the per-CPU
        // states, and then waited for any threads that were accessing the states to finish.
        //
        // It's now safe to destroy our CPU states. This will destroy the mappings and pinnings
        // that the kernel keeps to write to.
        self.set_state(SamplingState::Unallocated, token);
        // SAFETY: `ThreadSamplerLock` is held, `state` was transitioned from `Destroying` to
        // `Unallocated`, and `BufferRefCount` was verified to be 0 with `Ordering::Acquire`.
        unsafe {
            *self.per_cpu_buffers.get() = None;
        }
        Ok(())
    }

    /// Walks the current thread's user stack starting from `gregs` and writes a profiler
    /// backtrace record to the current CPU's buffer if sampling is enabled.
    ///
    /// Must stay out of line (`#[inline(never)]`): this puts a 512-byte backtrace buffer on the
    /// kernel stack, and it is reached only when a sampling session is running. Inlined into a
    /// caller on the exception path, that buffer would be charged to every exception's frame
    /// rather than to this shallow-stack call site.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - Interrupts are enabled (`!ints_disabled()`).
    /// - If `source` is [`GeneralRegsSource::Iframe`] or `GeneralRegsSource::Syscall`, `gregs` is
    ///   non-null, properly aligned, and points to a valid, initialized register frame of the
    ///   corresponding architecture type with no concurrent mutable aliases for the duration of
    ///   this call.
    #[inline(never)]
    pub unsafe fn sample_thread(
        &self,
        pid: zx_koid_t,
        tid: zx_koid_t,
        source: GeneralRegsSource,
        gregs: *const c_void,
        session_id: u64,
    ) -> Result<(), Status> {
        // We are going to attempt a usercopy below which might fault, so interrupts cannot be
        // disabled.
        debug_assert!(!ints_disabled());
        // We need to be a little bit careful here because we could be racing with a `stop`
        // operation. The `stop` operation:
        //
        // 1) Disables Writes
        // 2) Cancels each Timer
        // 3) Waits for all PendingWrites to finish
        //
        // It does this while holding `ThreadSamplerLock`. This means if we acquire a write ref
        // and then attempt to obtain `ThreadSamplerLock`, we could deadlock.
        //
        // Instead, we'll do a single enabled check here before attempting to read the stack, which
        // will take some time. Once we've collected our data and are ready to write out, we'll
        // acquire a `PerCpuBufferRef` to hold onto the buffers for the duration of the write.
        //
        // If we find that writes are enabled, we are safe to write to the buffers as `stop` will
        // not destroy them until we drop the `PerCpuBufferRef`.
        //
        // If we find that writes are disabled, we throw away our sample as it's no longer safe to
        // write to the buffers.
        if self.state() != SamplingState::Running {
            return Err(Status::BAD_STATE);
        }

        // If the session ids don't match, this sample request is one that got _way_ delayed from a
        // previous session.
        if session_id != self.session_id.load(Ordering::Relaxed) {
            return Err(Status::BAD_STATE);
        }

        let (mut fp, mut pc) = match source {
            GeneralRegsSource::None => (0usize, 0usize),
            GeneralRegsSource::Iframe => {
                debug_assert!(!gregs.is_null());
                // SAFETY: The caller guarantees `gregs` is non-null, properly aligned, and points
                // to a valid, initialized `Iframe` of `size_of::<Iframe>()` bytes with no
                // concurrent mutable aliases for the duration of this call.
                let iframe = unsafe { &*(gregs.cast::<crate::arch_rs::Iframe>()) };
                #[cfg(target_arch = "x86_64")]
                {
                    (iframe.rbp as usize, iframe.ip as usize)
                }
                #[cfg(target_arch = "aarch64")]
                {
                    (iframe.r[29] as usize, (iframe.elr as usize).wrapping_sub(4))
                }
                #[cfg(target_arch = "riscv64")]
                {
                    (iframe.regs.s0 as usize, iframe.regs.pc as usize)
                }
            }
            #[cfg(target_arch = "x86_64")]
            GeneralRegsSource::Syscall => {
                debug_assert!(!gregs.is_null());
                // SAFETY: The caller guarantees `gregs` is non-null, properly aligned, and points
                // to a valid, initialized `SyscallRegs` of `size_of::<SyscallRegs>()` bytes with
                // no concurrent mutable aliases for the duration of this call.
                let regs = unsafe { &*(gregs.cast::<crate::arch_rs::SyscallRegs>()) };
                (regs.rbp as usize, regs.rip as usize)
            }
        };

        if pc == 0 {
            return Err(Status::BAD_STATE);
        }

        const MAX_USER_BACKTRACE_SIZE: usize = 64;
        // We're dropping 512 bytes on the kernel stack here and we need to be careful not to
        // overflow it.
        //
        // This amount of bytes _should_ be safe because `sample_thread` is only called during
        // `Thread::Current::ProcessPendingSignals` which occurs directly before returning to
        // usermode. At this point, the stack will be shallow.
        //
        // That argument only holds while the buffer stays in this frame, which is why
        // `sample_thread` is declared `#[inline(never)]`. Inlined into a caller on the exception
        // path, these 512 bytes would be allocated on entry to every exception, at whatever depth
        // it is taken, and whether or not a sampling session is running.
        let mut bt = [const { MaybeUninit::<usize>::uninit() }; MAX_USER_BACKTRACE_SIZE];
        let mut frame_num = 0usize;
        bt[frame_num].write(pc);
        frame_num += 1;

        while frame_num < MAX_USER_BACKTRACE_SIZE {
            if fp == 0 {
                // We've reached the top of the frame pointer chain.
                break;
            }

            // RISC-V has a nonstandard frame pointer which points to the CFA instead of the
            // previous frame pointer. Since the frame pointer and return address are always just
            // below the CFA, subtract 16 bytes to get to the actual frame pointer.
            #[cfg(target_arch = "riscv64")]
            let actual_fp = fp.wrapping_sub(16);
            #[cfg(not(target_arch = "riscv64"))]
            let actual_fp = fp;

            let user_next_fp = UserInPtr::<usize>::new(without_provenance(actual_fp));
            let user_pc = UserInPtr::<usize>::new(without_provenance(actual_fp.wrapping_add(8)));

            // Code without frame pointers (e.g. JIT or AOT compiled managed code) can leave a
            // frame pointer that points to unreadable or non-stack memory. The frames we've
            // already collected, including the leaf, are still valid, so record a truncated
            // backtrace rather than discarding the whole sample.
            let Ok(next_pc) = user_pc.read() else {
                break;
            };
            pc = next_pc;
            if pc == 0 || !crate::arch_rs::vm::is_user_accessible(pc) {
                break;
            }
            bt[frame_num].write(pc);
            frame_num += 1;

            let Ok(next_fp) = user_next_fp.read() else {
                break;
            };
            fp = next_fp;
        }

        // Up until this point, interrupts are enabled so that we can handle faults when doing
        // usercopies. However, once we want to write, we need to disable interrupts as the
        // underlying buffer writing algorithm assumes a single writer. We ensure we don't get
        // interrupted or context switched while we are writing. Otherwise, we could get context
        // switched out, and then have another thread attempt to write.
        let irqd = InterruptDisableGuard::new();

        let Some(token) = self.get_buffer_ref_for_writing(curr_cpu_num()) else {
            return Err(Status::BAD_STATE);
        };
        let cpu_state = token.get();

        // SAFETY: `bt[..frame_num]` have all been initialized sequentially via `MaybeUninit::write`
        // above, and `MaybeUninit<usize>` has the same layout as `usize`.
        let bt_init =
            unsafe { core::slice::from_raw_parts(bt.as_ptr().cast::<usize>(), frame_num) };

        // Drop the record if we fail to write out.
        // SAFETY: `token` is for `curr_cpu_num()` and `irqd` disables interrupts on this CPU,
        // guaranteeing single-producer exclusivity on `cpu_state`.
        let _ = unsafe {
            write_profiler_backtrace_record(
                cpu_state,
                &irqd,
                timer_current_mono_ticks().0 as u64,
                pid,
                tid,
                bt_init,
            )
        };
        Ok(())
    }

    /// Copies the contents of all per-CPU buffers into user memory `ptr` of length `len`,
    /// returning `(status, bytes_read)`.
    ///
    /// If `ptr` is null, returns `(Ok(()), total_required_buffer_size)` without copying.
    pub fn read_user(&self, ptr: UserOutPtr<u8>, len: usize) -> (Result<(), Status>, usize) {
        // We unfortunately run into some complexity here: while the buffer our samples are in is
        // created by the kernel and is safe to read from, the user memory we are writing to could
        // be pager-backed. This means that when we attempt to write to it as part of the
        // `VmObjectPaged::ReadUser` call, we cannot be holding locks.
        //
        // During the copy, we'd need to prevent:
        //   1) The buffers being destroyed due to the read handle being `zx_handle_close`'d
        //   2) A new sampler from being created.
        //
        // We do this by atomically incrementing a ref count on the buffers if they are safe to
        // read, then only allowing the buffers to be destroyed and the sampler state to advance
        // once we release the ref count.
        lockdep::assert_no_locks_held();

        let Some(ref_token) = self.get_buffer_ref_for_reading() else {
            return (Err(Status::BAD_STATE), 0);
        };
        let per_cpu_buffers = ref_token.get();

        let num_buffers = per_cpu_buffers.len();
        // All buffers are the same size.
        let total_size =
            per_cpu_buffers.first().map_or(0, |b| (b.size() as usize).wrapping_mul(num_buffers));

        // The caller can query the required buffer size by passing in a nullptr.
        if ptr.is_null() {
            return (Ok(()), total_size);
        }

        // Eventually, this should support users passing in buffers smaller than the sum of the
        // size of all per-CPU buffers, but for now we do not allow this.
        if len < total_size {
            return (Err(Status::INVALID_ARGS), 0);
        }

        let Some(_consumer_guard) = ReadConsumerGuard::try_acquire(&self.reading) else {
            return (Err(Status::BAD_STATE), 0);
        };

        // Iterate through each per-CPU buffer and read its contents.
        let mut bytes_read: usize = 0;
        for buf in per_cpu_buffers {
            let copy_fn = |byte_offset: u32, src: &[u8]| -> Result<(), Status> {
                let out_ptr =
                    ptr.byte_offset(bytes_read.wrapping_add(byte_offset as usize) as isize);
                out_ptr.copy_slice_to_user(src)
            };
            match buf.read(copy_fn, buf.size()) {
                Ok(read) => {
                    bytes_read = bytes_read.wrapping_add(read as usize);
                }
                Err(status) => {
                    // If we copied some data from a previous buffer, we have to return the fact
                    // that we did so here. Otherwise, that data will be lost.
                    return (Err(status), bytes_read);
                }
            }
        }
        (Ok(()), bytes_read)
    }

    /// Atomically schedules the sampling timer on the current CPU if the session is
    /// [`SamplingState::Running`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
    unsafe fn schedule_marking(&self) {
        debug_assert!(ints_disabled());

        // `sampler_percpu_init` and an IPI from `ThreadSampler::start` might race if we attempt to
        // start a session while a core is being onlined. We handle this race by clearing our state
        // out first then resetting it.
        //
        // `cancel_marking` is safe to call even if no timer is set -- it'll only decrement the
        // refcount if a timer was actually canceled.
        // SAFETY: The caller guarantees interrupts are disabled on the current CPU.
        unsafe { self.cancel_marking() };

        let mut expected = self.state.load(Ordering::SeqCst);
        // We only want to schedule a sample while we are running.
        loop {
            let s = SamplingState::from_raw((expected & Self::STATE_MASK) as u8);
            if s != SamplingState::Running {
                return;
            }
            if (expected & Self::MARKINGS_SCHEDULED_MASK) == Self::MARKINGS_SCHEDULED_MASK {
                debug_assert!(
                    (expected & Self::MARKINGS_SCHEDULED_MASK) != Self::MARKINGS_SCHEDULED_MASK
                );
                return;
            }
            let desired = expected.wrapping_add(Self::MARKINGS_SCHEDULED_INCREMENT);
            match self.state.compare_exchange_weak(
                expected,
                desired,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => expected = actual,
            }
        }

        let deadline = Deadline::after_mono(
            DurationMono(self.sample_period.load(Ordering::Relaxed)),
            TimerSlack::none(),
        );
        let session_id = self.session_id.load(Ordering::Relaxed);

        // SAFETY:
        // - `PerCpu::get_current().sampling_timer` resides in permanently pinned `'static` per-CPU
        //   storage.
        // - Interrupts are disabled on the current CPU, preventing CPU migration and serializing
        //   all `set_deadline` and `cancel` calls on this CPU's `sampling_timer`.
        // - `cpp_thread_signal_sample_stack` is a valid FFI timer callback, and `session_id` is
        //   passed by value encoded via `without_provenance_mut`.
        unsafe {
            Pin::static_ref(&PerCpu::get_current().sampling_timer).set_deadline(
                &deadline,
                cpp_thread_signal_sample_stack,
                without_provenance_mut(session_id as usize),
            );
        }
    }

    /// Re-arms the sampling timer on the current CPU if the session is still
    /// [`SamplingState::Running`], or decrements `MarkingsScheduled` if the session has stopped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - Interrupts are disabled on the current CPU (`ints_disabled()`).
    /// - This function is called at most once per firing of the current CPU's `sampling_timer`
    ///   callback, representing an active scheduled marking in `MarkingsScheduled`.
    pub unsafe fn reschedule_marking(&self) {
        debug_assert!(ints_disabled());
        let expected = self.state.load(Ordering::Acquire);
        let s = SamplingState::from_raw((expected & Self::STATE_MASK) as u8);
        if s != SamplingState::Running {
            let result = self.state.fetch_sub(Self::MARKINGS_SCHEDULED_INCREMENT, Ordering::AcqRel);
            debug_assert!((result & Self::MARKINGS_SCHEDULED_MASK) != 0);
            return;
        }
        let deadline = Deadline::after_mono(
            DurationMono(self.sample_period.load(Ordering::Relaxed)),
            TimerSlack::none(),
        );
        let session_id = self.session_id.load(Ordering::Relaxed);
        // SAFETY:
        // - `PerCpu::get_current().sampling_timer` resides in permanently pinned `'static` per-CPU
        //   storage.
        // - Interrupts are disabled inside the timer callback on the current CPU, preventing CPU
        //   migration and serializing `set_deadline` and `cancel` on this CPU's `sampling_timer`.
        // - `cpp_thread_signal_sample_stack` is a valid FFI timer callback, and `session_id` is
        //   passed by value encoded via `without_provenance_mut`.
        unsafe {
            Pin::static_ref(&PerCpu::get_current().sampling_timer).set_deadline(
                &deadline,
                cpp_thread_signal_sample_stack,
                without_provenance_mut(session_id as usize),
            );
        }
    }

    /// Cancels the sampling timer on the current CPU and decrements `MarkingsScheduled` if a
    /// pending timer was canceled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
    unsafe fn cancel_marking(&self) {
        debug_assert!(ints_disabled());
        let cancelled = PerCpu::get_current().sampling_timer.cancel();
        if cancelled {
            let prev = self.state.fetch_sub(Self::MARKINGS_SCHEDULED_INCREMENT, Ordering::SeqCst);
            debug_assert!((prev & Self::MARKINGS_SCHEDULED_MASK) != 0);
        }
    }
}

/// Unit tests for [`ThreadSampler`].
#[cfg(ktest)]
#[unittest::suite(name = "thread_sampler")]
mod tests {
    use core::mem::{MaybeUninit, size_of};
    use core::sync::atomic::Ordering;

    use fxt_layout::ProfilerRecordType;
    use page::SIZE as PAGE_SIZE;
    use unittest::{
        assert_eq, assert_false, assert_ok, assert_true, expect_eq, expect_ge, expect_le,
    };
    use zerocopy::IntoBytes;
    use zx_types::{zx_koid_t, zx_sampler_config_t};

    use super::{
        InterruptDisableGuard, MpIpiTarget, PerCpu, RecordHeader, RecordType, SamplingState,
        THREAD_SAMPLER, ThreadSampler, ThreadSamplerLock, curr_cpu_num, sync_exec,
        timer_current_mono_ticks, write_profiler_backtrace_record, zx_msec,
    };
    use crate::kernel::mp::get_online_mask;
    use crate::user_copy::UserOutPtr;
    use crate::user_memory::UserMemory;

    const ONE_MSEC_NS: i64 = zx_msec(1);

    fn get_buffer_ref_count(sampler: &ThreadSampler) -> u64 {
        let state = sampler.state.load(Ordering::Relaxed);
        (state & ThreadSampler::BUFFER_REF_COUNT_MASK) >> ThreadSampler::BUFFER_REF_COUNT_SHIFT
    }

    fn get_timer_ref_count(sampler: &ThreadSampler) -> u64 {
        let state = sampler.state.load(Ordering::Relaxed);
        (state & ThreadSampler::MARKINGS_SCHEDULED_MASK) >> ThreadSampler::MARKINGS_SCHEDULED_SHIFT
    }

    fn write_synthetic_sample(sampler: &ThreadSampler, pid: zx_koid_t, tid: zx_koid_t) {
        let irqd = InterruptDisableGuard::new();
        let Some(buffers) = sampler.get_buffer_ref_for_writing(curr_cpu_num()) else {
            return;
        };
        let cpu_state = buffers.get();

        const MAX_USER_BACKTRACE_SIZE: usize = 64;
        let mut bt = [0usize; MAX_USER_BACKTRACE_SIZE];
        for (i, slot) in bt.iter_mut().enumerate() {
            *slot = i;
        }

        // SAFETY: `buffers` is for `curr_cpu_num()` and `irqd` disables interrupts on this CPU.
        let _ = unsafe {
            write_profiler_backtrace_record(
                cpu_state,
                &irqd,
                timer_current_mono_ticks().0 as u64,
                pid,
                tid,
                &bt,
            )
        };
    }

    fn expected_backtrace_header(num_words: u16, num_frames: u64) -> u64 {
        let mut hdr = RecordHeader::new();
        hdr.set_record_type(RecordType::Profiler).set_record_size(num_words);
        hdr.bits() | ((ProfilerRecordType::Backtrace as u64) << 16) | (num_frames << 28)
    }

    /// Repeatedly sets up, starts, stops, and destroys the global thread sampler.
    #[test]
    fn repeat_start_stop_test() {
        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        for _ in 0..10 {
            assert_true!(THREAD_SAMPLER.set_up(&config).is_ok());
            assert_true!(THREAD_SAMPLER.start().is_ok());
            assert_true!(THREAD_SAMPLER.stop().is_ok());
            assert_true!(THREAD_SAMPLER.destroy().is_ok());
        }
    }

    /// Writes synthetic samples on all online CPUs and verifies the serialized FXT records.
    #[test]
    fn write_sample_test() {
        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        let test_state = &THREAD_SAMPLER;
        assert_ok!(test_state.set_up(&config));
        assert_ok!(test_state.start());

        let before = timer_current_mono_ticks().0 as u64;
        sync_exec(MpIpiTarget::All, 0, || {
            write_synthetic_sample(test_state, curr_cpu_num() as u64, 1);
        });
        let after = timer_current_mono_ticks().0 as u64;
        assert_ok!(test_state.stop());

        let num_cpus = PerCpu::processor_count();
        let read_token = test_state.get_buffer_ref_for_reading().unwrap();
        let buffers = read_token.get();
        for (i, buf) in buffers.iter().enumerate().take(num_cpus) {
            /// `num_words` = 64 backtrace + 1 header + 1 ts + 1 pid + 1 tid = 68.
            const NUM_WORDS: usize = 68;
            let expected_header = expected_backtrace_header(NUM_WORDS as u16, 64);
            let mut record = [0u64; NUM_WORDS];
            let record_bytes_len = size_of::<[u64; NUM_WORDS]>();
            let read_result = buf.read(
                |offset, data| {
                    let dst = record.as_mut_bytes();
                    let start = offset as usize;
                    dst[start..start + data.len()].copy_from_slice(data);
                    Ok(())
                },
                record_bytes_len as u32,
            );
            assert_true!(read_result.is_ok());
            assert_eq!(read_result.unwrap() as usize, record_bytes_len);

            expect_eq!(expected_header, record[0]);
            expect_ge!(record[1], before);
            expect_le!(record[1], after);
            expect_eq!(i as u64, record[2]);
            expect_eq!(1u64, record[3]);
            for frame in 0..64usize {
                expect_eq!(record[4 + frame], frame as u64);
            }
        }
        drop(read_token);
        assert_ok!(test_state.destroy());
    }

    /// Verifies that state transitions preserve the in-flight buffer reference count.
    #[test]
    fn state_change() {
        let sampler = &THREAD_SAMPLER;
        assert_eq!(0u64, sampler.state.load(Ordering::Relaxed));
        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        assert_ok!(sampler.set_up(&config));
        assert_eq!(SamplingState::Configured, sampler.state());
        assert_true!(sampler.start().is_ok());
        assert_eq!(SamplingState::Running, sampler.state());

        let write_ref = sampler.get_buffer_ref_for_writing(0);
        assert_true!(write_ref.is_some());
        assert_eq!(SamplingState::Running, sampler.state());
        assert_eq!(1u64, get_buffer_ref_count(sampler));

        // Changing the state shouldn't change the ref count.
        {
            ksync::lock!(let guard = ThreadSamplerLock::lock());
            sampler.set_state(SamplingState::Stopping, guard.token());
        }
        assert_eq!(SamplingState::Stopping, sampler.state());
        assert_eq!(1u64, get_buffer_ref_count(sampler));
        drop(write_ref);

        // Restore state to Running so `stop_locked()` runs during `destroy()` to cancel active
        // timers.
        {
            ksync::lock!(let guard = ThreadSamplerLock::lock());
            sampler.set_state(SamplingState::Running, guard.token());
        }
        assert_ok!(sampler.destroy());
    }

    /// Verifies `get_buffer_ref_for_writing` across all sampler states and CPU bounds.
    #[test]
    fn acquire_buffers() {
        let sampler = &THREAD_SAMPLER;
        let max_cpus = PerCpu::processor_count() as u32;
        for i in 0..=max_cpus {
            assert_false!(sampler.get_buffer_ref_for_writing(i).is_some());
        }

        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        assert_ok!(sampler.set_up(&config));

        // Cannot get write buffers unless Running.
        for i in 0..=max_cpus {
            assert_false!(sampler.get_buffer_ref_for_writing(i).is_some());
        }
        assert_true!(sampler.start().is_ok());

        for i in 0..max_cpus {
            assert_true!(sampler.get_buffer_ref_for_writing(i).is_some());
        }
        assert_false!(sampler.get_buffer_ref_for_writing(max_cpus).is_some());

        assert_true!(sampler.stop().is_ok());
        for i in 0..=max_cpus {
            assert_false!(sampler.get_buffer_ref_for_writing(i).is_some());
        }
        assert_true!(sampler.destroy().is_ok());
        for i in 0..=max_cpus {
            assert_false!(sampler.get_buffer_ref_for_writing(i).is_some());
        }
    }

    /// Verifies that per-CPU timer references match the online CPU count during a session.
    #[test]
    fn timer_references() {
        let test_state = &THREAD_SAMPLER;
        let num_cpus_online = u64::from(get_online_mask().count_ones());
        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        assert_ok!(test_state.set_up(&config));
        assert_eq!(0u64, get_timer_ref_count(test_state));

        assert_ok!(test_state.start());
        assert_eq!(num_cpus_online, get_timer_ref_count(test_state));

        assert_ok!(test_state.stop());
        assert_eq!(0u64, get_timer_ref_count(test_state));
        assert_ok!(test_state.destroy());
    }

    // TODO(https://fxbug.dev/511205186): Re-enable these once hotplugging flakes have been fixed.
    // The CPU hotplug tests (`cpu_offlining` and `cpu_hotplug_multithreaded`) and their helpers
    // (`check_timer_refs_is`, `wait_for_cpu_offline`, `unplug_cpus`, `plug_cpus`) are retained
    // below under `#[cfg(false)]` until https://fxbug.dev/511205186 is resolved.
    #[cfg(false)]
    mod hotplug_tests {
        use super::*;
        use crate::kernel::mp::{cpu_num_to_mask, remove_cpu_from_mask};
        use crate::kernel::thread::migrate_to_cpu;
        use crate::kernel::types::{BOOT_CPU_ID, INVALID_CPU, cpu_mask_t, cpu_num_t};
        use crate::platform_rs::timer::{DurationMono, current_mono_time, zx_sec, zx_usec};
        use zx_status::Status;

        fn check_timer_refs_is(test_state: &ThreadSampler, expected: u64) -> Result<(), Status> {
            let end_time = current_mono_time().0.saturating_add(zx_sec(5));
            while current_mono_time().0 < end_time {
                if expected == get_timer_ref_count(test_state) {
                    return Ok(());
                }
                let _ = super::super::sleep_relative(DurationMono(zx_usec(200)));
            }
            Err(Status::TIMED_OUT)
        }

        fn unplug_cpus(_cpumask: cpu_mask_t) -> Result<(), Status> {
            Err(Status::NOT_SUPPORTED)
        }

        fn plug_cpus(_cpumask: cpu_mask_t) -> Result<(), Status> {
            Err(Status::NOT_SUPPORTED)
        }

        /// Start/Stop/Read while offlining CPUs.
        #[test]
        fn cpu_offlining() {
            #[cfg(target_arch = "riscv64")]
            {
                return;
            }
            let num_cpus_online = u64::from(get_online_mask().count_ones());
            if num_cpus_online < 2 {
                return;
            }
            // SAFETY: `BOOT_CPU_ID` is online and no spinlocks are held.
            unsafe { migrate_to_cpu(BOOT_CPU_ID) };
            let config = zx_sampler_config_t {
                period: ONE_MSEC_NS,
                buffer_size: PAGE_SIZE,
                iobuffer_discipline: 0,
            };
            let test_state = &THREAD_SAMPLER;
            let original_online_mask = get_online_mask();
            let secondary_cpus = original_online_mask & !cpu_num_to_mask(BOOT_CPU_ID);

            assert_ok!(test_state.set_up(&config));
            assert_ok!(test_state.start());
            assert_ok!(check_timer_refs_is(test_state, num_cpus_online));
            assert_ok!(unplug_cpus(secondary_cpus));
            assert_ok!(check_timer_refs_is(test_state, 1));
            assert_ok!(plug_cpus(secondary_cpus));
            assert_ok!(check_timer_refs_is(test_state, num_cpus_online));
            assert_ok!(test_state.stop());
            assert_ok!(test_state.destroy());
        }

        /// Sampler is thread-safe wrt hotplugging.
        #[test]
        fn cpu_hotplug_multithreaded() {
            #[cfg(target_arch = "riscv64")]
            {
                return;
            }
            let num_cpus_online = u64::from(get_online_mask().count_ones());
            if num_cpus_online < 2 {
                return;
            }
            let config = zx_sampler_config_t {
                period: ONE_MSEC_NS,
                buffer_size: PAGE_SIZE,
                iobuffer_discipline: 0,
            };
            let test_state = &THREAD_SAMPLER;
            for _ in 0..10 {
                assert_ok!(test_state.set_up(&config));
                assert_ok!(check_timer_refs_is(test_state, 0));
                assert_ok!(test_state.start());
                let _ = super::super::sleep_relative(DurationMono(zx_msec(5)));
                assert_ok!(test_state.stop());
                assert_ok!(check_timer_refs_is(test_state, 0));
                assert_ok!(test_state.destroy());
                assert_ok!(check_timer_refs_is(test_state, 0));
            }
        }
    }

    /// Verifies `get_buffer_ref_for_reading` across all sampler states.
    #[test]
    fn acquire_buffers_reading() {
        let sampler = &THREAD_SAMPLER;
        assert_false!(sampler.get_buffer_ref_for_reading().is_some());

        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        assert_ok!(sampler.set_up(&config));
        assert_true!(sampler.get_buffer_ref_for_reading().is_some());

        assert_ok!(sampler.start());
        assert_true!(sampler.get_buffer_ref_for_reading().is_some());

        assert_ok!(sampler.stop());
        assert_true!(sampler.get_buffer_ref_for_reading().is_some());

        assert_ok!(sampler.destroy());
        assert_false!(sampler.get_buffer_ref_for_reading().is_some());
    }

    /// Verifies `read_user` while a sampling session is actively running.
    #[test]
    fn reading_during_session() {
        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        let sampler = &THREAD_SAMPLER;
        assert_ok!(sampler.set_up(&config));
        assert_ok!(sampler.start());

        let before = timer_current_mono_ticks().0 as u64;
        sync_exec(MpIpiTarget::All, 0, || {
            write_synthetic_sample(&THREAD_SAMPLER, curr_cpu_num() as u64, 1);
        });
        let after = timer_current_mono_ticks().0 as u64;

        const NUM_WORDS: usize = 68;
        let (query_status, expected_size) = sampler.read_user(UserOutPtr::default(), 0);
        assert_ok!(query_status);
        let user = UserMemory::create(expected_size).unwrap();
        let (status, bytes_read) = sampler.read_user(user.user_out::<u8>(), expected_size);
        assert_ok!(status);

        let num_cpus_online = get_online_mask().count_ones() as usize;
        let record_size = NUM_WORDS * size_of::<u64>();
        assert_eq!(num_cpus_online * record_size, bytes_read);

        for i in 0..num_cpus_online {
            let mut raw_words = [MaybeUninit::<u64>::uninit(); NUM_WORDS];
            // SAFETY: `MaybeUninit<u64>` has the same layout as 8 `MaybeUninit<u8>` bytes.
            let raw_bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    raw_words.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    record_size,
                )
            };
            let _ = user.vmo_read(raw_bytes, (i * record_size) as u64).unwrap();
            // SAFETY: `vmo_read` succeeded and initialized all `record_size` bytes of `raw_words`.
            let record: &[u64; NUM_WORDS] =
                unsafe { &*raw_words.as_ptr().cast::<[u64; NUM_WORDS]>() };

            let expected_header = expected_backtrace_header(NUM_WORDS as u16, 64);
            expect_eq!(expected_header, record[0]);
            expect_ge!(record[1], before);
            expect_le!(record[1], after);
            expect_eq!(i as u64, record[2]);
            expect_eq!(1u64, record[3]);
            for frame in 0..64usize {
                expect_eq!(record[4 + frame], frame as u64);
            }
        }

        // Reading again should yield no data.
        let (empty_status, empty_read) = sampler.read_user(user.user_out::<u8>(), expected_size);
        assert_ok!(empty_status);
        assert_eq!(0usize, empty_read);

        assert_ok!(sampler.stop());
        assert_ok!(sampler.destroy());
    }

    /// Verifies that truncated backtraces keep valid prefix frames and stop at non-user PCs.
    #[test]
    fn truncated_backtrace() {
        use super::GeneralRegsSource;
        use crate::arch_rs::{Iframe, USER_ASPACE_BASE};
        use crate::kernel::mp::cpu_num_to_mask;
        use crate::kernel::thread::ThreadPtr;
        use crate::kernel::types::{BOOT_CPU_ID, cpu_mask_t};

        struct RestoreAffinity(cpu_mask_t);
        impl Drop for RestoreAffinity {
            fn drop(&mut self) {
                // SAFETY: Restoring the current thread's previous CPU affinity mask.
                let _ = unsafe { ThreadPtr::current().set_cpu_affinity(self.0) };
            }
        }
        // SAFETY: Pinning the current thread to `BOOT_CPU_ID` for the duration of the test.
        let _restore_affinity = RestoreAffinity(unsafe {
            ThreadPtr::current().set_cpu_affinity(cpu_num_to_mask(BOOT_CPU_ID))
        });

        let config = zx_sampler_config_t {
            period: ONE_MSEC_NS,
            buffer_size: PAGE_SIZE,
            iobuffer_discipline: 0,
        };
        let sampler = &THREAD_SAMPLER;
        assert_ok!(sampler.set_up(&config));
        struct CleanupSampler;
        impl Drop for CleanupSampler {
            fn drop(&mut self) {
                {
                    ksync::lock!(let guard = ThreadSamplerLock::lock());
                    THREAD_SAMPLER.set_state(SamplingState::Configured, guard.token());
                }
                let _ = THREAD_SAMPLER.destroy();
            }
        }
        let _cleanup = CleanupSampler;
        {
            ksync::lock!(let guard = ThreadSamplerLock::lock());
            sampler.set_state(SamplingState::Running, guard.token());
        }

        let stack = UserMemory::create(PAGE_SIZE).unwrap();
        assert_ok!(stack.commit_and_map(0..PAGE_SIZE));

        let encode_fp = |frame_addr: usize| -> usize {
            #[cfg(target_arch = "riscv64")]
            {
                frame_addr.wrapping_add(2 * size_of::<usize>())
            }
            #[cfg(not(target_arch = "riscv64"))]
            {
                frame_addr
            }
        };

        let make_iframe = |pc: usize, fp: usize| -> Iframe {
            // SAFETY: `Iframe` is a plain-old-data register struct valid when zero-initialized.
            let mut iframe: Iframe = unsafe { MaybeUninit::zeroed().assume_init() };
            #[cfg(target_arch = "x86_64")]
            {
                iframe.rbp = fp as u64;
                iframe.ip = pc as u64;
            }
            #[cfg(target_arch = "aarch64")]
            {
                iframe.r[29] = fp as u64;
                iframe.elr = (pc as u64).wrapping_add(4);
            }
            #[cfg(target_arch = "riscv64")]
            {
                iframe.regs.s0 = fp as u64;
                iframe.regs.pc = pc as u64;
            }
            iframe
        };

        const LEAF_PC: usize = USER_ASPACE_BASE + 0x1000;
        const CALLER1_PC: usize = USER_ASPACE_BASE + 0x2000;
        const CALLER2_PC: usize = USER_ASPACE_BASE + 0x3000;
        let frame0_addr = stack.base() + 0x100;
        let frame1_addr = stack.base() + 0x120;

        // 1. Frame pointer chain ending in an unmapped address: the valid prefix frames must be
        // kept.
        assert_ok!(stack.put::<usize>(
            encode_fp(frame1_addr),
            (frame0_addr - stack.base()) / size_of::<usize>(),
        ));
        assert_ok!(
            stack
                .put::<usize>(CALLER1_PC, ((frame0_addr - stack.base()) / size_of::<usize>()) + 1,)
        );
        assert_ok!(stack.put::<usize>(
            encode_fp(0x1000),
            (frame1_addr - stack.base()) / size_of::<usize>(),
        ));
        assert_ok!(
            stack
                .put::<usize>(CALLER2_PC, ((frame1_addr - stack.base()) / size_of::<usize>()) + 1,)
        );

        let iframe = make_iframe(LEAF_PC, encode_fp(frame0_addr));
        let session_id = sampler.session_id.load(Ordering::Relaxed);
        // SAFETY: Interrupts are enabled and `iframe` is a valid initialized `Iframe`.
        assert_ok!(unsafe {
            sampler.sample_thread(
                1,
                2,
                GeneralRegsSource::Iframe,
                (&raw const iframe).cast(),
                session_id,
            )
        });

        let read_token = sampler.get_buffer_ref_for_reading().unwrap();
        let cpu_buffer = &read_token.get()[BOOT_CPU_ID as usize];
        {
            let mut record = [0u64; 7];
            let record_bytes_len = size_of::<[u64; 7]>();
            let read_result = cpu_buffer.read(
                |offset, data| {
                    let dst = record.as_mut_bytes();
                    let start = offset as usize;
                    dst[start..start + data.len()].copy_from_slice(data);
                    Ok(())
                },
                record_bytes_len as u32,
            );
            assert_true!(read_result.is_ok());
            assert_eq!(record_bytes_len, read_result.unwrap() as usize);
            expect_eq!(LEAF_PC as u64, record[4]);
            expect_eq!(CALLER1_PC as u64, record[5]);
            expect_eq!(CALLER2_PC as u64, record[6]);
        }

        // 2. Frame containing a non-user-accessible PC (such as machine code read from .text):
        // garbage frames must not be recorded.
        assert_ok!(stack.put::<usize>(0, (frame1_addr - stack.base()) / size_of::<usize>()));
        assert_ok!(stack.put::<usize>(
            0x4106_4a17_e7d7_1be7usize,
            ((frame1_addr - stack.base()) / size_of::<usize>()) + 1,
        ));

        let bad_pc_iframe = make_iframe(LEAF_PC, encode_fp(frame1_addr));
        // SAFETY: Interrupts are enabled and `bad_pc_iframe` is a valid initialized `Iframe`.
        assert_ok!(unsafe {
            sampler.sample_thread(
                1,
                2,
                GeneralRegsSource::Iframe,
                (&raw const bad_pc_iframe).cast(),
                session_id,
            )
        });
        {
            let mut record = [0u64; 5];
            let record_bytes_len = size_of::<[u64; 5]>();
            let read_result = cpu_buffer.read(
                |offset, data| {
                    let dst = record.as_mut_bytes();
                    let start = offset as usize;
                    dst[start..start + data.len()].copy_from_slice(data);
                    Ok(())
                },
                record_bytes_len as u32,
            );
            assert_true!(read_result.is_ok());
            assert_eq!(record_bytes_len, read_result.unwrap() as usize);
            expect_eq!(LEAF_PC as u64, record[4]);
        }
    }
}
