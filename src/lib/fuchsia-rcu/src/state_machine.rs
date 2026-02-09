// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic_stack::AtomicStack;
use fuchsia_sync::Mutex;
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::thread_local;

#[cfg(feature = "rseq_backend")]
use crate::read_counters::RcuReadCounters;

type RcuCallback = Box<dyn FnOnce() + Send + Sync + 'static>;

/// The length of the queue of waiting callbacks.
///
/// The state machine waits for this many generations to complete before running these callbacks.
const QUEUE_LENGTH: usize = 2;

/// The queue of waiting callbacks.
///
/// The queue is a ring buffer of sets of callbacks of length `QUEUE_LENGTH`.
struct CallbackQueue {
    /// The callbacks that are waiting to be run.
    ///
    /// The callbacks are stored in a ring buffer.
    callbacks: [Vec<RcuCallback>; QUEUE_LENGTH],
}

impl CallbackQueue {
    /// Create an empty callback queue.
    const fn new() -> Self {
        Self { callbacks: [Vec::new(), Vec::new()] }
    }

    /// Add a set of callbacks to the queue for the given generation.
    ///
    /// # Panics
    ///
    /// Panics if the slot for the given generation is already occupied.
    fn enqueue(&mut self, generation: usize, callbacks: Vec<RcuCallback>) {
        let index = generation % QUEUE_LENGTH;
        assert!(
            self.callbacks[index].is_empty(),
            "Queue slot for generation {} is already occupied",
            generation
        );
        self.callbacks[index] = callbacks;
    }

    /// Pops the set of callbacks that are ready to be run after the given generation completes.
    fn take_ready(&mut self, generation: usize) -> Vec<RcuCallback> {
        // We take the callbacks that have reached the end of the queue, which is the same as the
        // slot in the queue that the next generation will occupy.
        let index = (generation + 1) % QUEUE_LENGTH;
        std::mem::take(&mut self.callbacks[index])
    }
}

struct RcuControlBlock {
    /// The generation counter.
    ///
    /// The generation counter is incremented whenever the state machine leaves the `Idle` state.
    generation: AtomicUsize,

    /// The read counters.
    ///
    /// Readers increment the counter for the generation that they are reading from. For example,
    /// if the `generation` is even, then readers increment the counter for the `read_counters[0]`.
    /// If the `generation` is odd, then readers increment the counter for the `read_counters[1]`.
    #[cfg(not(feature = "rseq_backend"))]
    read_counters: [AtomicUsize; 2],

    #[cfg(feature = "rseq_backend")]
    read_counters: RcuReadCounters,

    /// The chain of callbacks that are waiting to be run.
    ///
    /// Writers add callbacks to this chain after writing to the object. The callbacks are run when
    /// all currently in-flight read operations have completed.
    callback_chain: AtomicStack<RcuCallback>,

    /// The futex used to wait for the state machine to advance.
    advancer: zx::Futex,

    /// The queue of waiting callbacks.
    ///
    /// Callbacks are added to this queue when the state machine leaves the `Idle` state. They are
    /// run when the state machine leaves the `Waiting` state after `QUEUE_LENGTH` generations
    /// have completed.
    waiting_callbacks: Mutex<CallbackQueue>,
}

const ADVANCER_IDLE: i32 = 0;
const ADVANCER_WAITING: i32 = 1;

impl RcuControlBlock {
    /// Create a new control block for the RCU state machine.
    const fn new() -> Self {
        #[cfg(feature = "rseq_backend")]
        let read_counters = RcuReadCounters::new();

        #[cfg(not(feature = "rseq_backend"))]
        let read_counters = [AtomicUsize::new(0), AtomicUsize::new(0)];

        Self {
            generation: AtomicUsize::new(0),
            read_counters,
            callback_chain: AtomicStack::new(),
            advancer: zx::Futex::new(ADVANCER_IDLE),
            waiting_callbacks: Mutex::new(CallbackQueue::new()),
        }
    }
}

/// The control block for the RCU state machine.
static RCU_CONTROL_BLOCK: RcuControlBlock = RcuControlBlock::new();

#[derive(Default)]
struct RcuThreadBlock {
    /// The number of times the thread has nested into a read lock.
    nesting_level: Cell<usize>,

    /// The index of the read counter that the thread incremented when it entered its outermost read
    /// lock.
    counter_index: Cell<u8>,

    /// Whether this thread has scheduled callbacks since the last time the thread called
    /// `rcu_synchronize`.
    has_pending_callbacks: Cell<bool>,
}

impl RcuThreadBlock {
    /// Returns true if the thread is holding a read lock.
    fn holding_read_lock(&self) -> bool {
        self.nesting_level.get() > 0
    }
}

thread_local! {
    /// Thread-specific data for the RCU state machine.
    ///
    /// This data is used to track the nesting level of read locks and the index of the read counter
    /// that the thread incremented when it entered its outermost read lock.
    static RCU_THREAD_BLOCK: RcuThreadBlock = RcuThreadBlock::default();
}

/// Acquire a read lock.
///
/// This function is used to acquire a read lock on the RCU state machine. The RCU state machine
/// defers calling callbacks until all currently in-flight read operations have completed.
///
/// Must be balanced by a call to `rcu_read_unlock` on the same thread.
pub(crate) fn rcu_read_lock() {
    RCU_THREAD_BLOCK.with(|thread_block| {
        let nesting_level = thread_block.nesting_level.get();
        if nesting_level > 0 {
            // If this thread already has a read lock, increment the nesting level instead of the
            // incrementing the read counter. This approach is a performance optimization to reduce
            // the number of atomic operations that need to be performed.
            thread_block.nesting_level.set(nesting_level + 1);
        } else {
            // This is the outermost read lock. Increment the read counter.
            let control_block = &RCU_CONTROL_BLOCK;
            let index = control_block.generation.load(Ordering::Relaxed) & 1;

            #[cfg(feature = "rseq_backend")]
            {
                control_block.read_counters.begin(index);
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
            }

            #[cfg(not(feature = "rseq_backend"))]
            {
                // Synchronization point [A] (see design.md)
                control_block.read_counters[index].fetch_add(1, Ordering::SeqCst);
            }

            thread_block.counter_index.set(index as u8);
            thread_block.nesting_level.set(1);
        }
    });
}

/// Release a read lock.
///
/// This function is used to release a read lock on the RCU state machine. See `rcu_read_lock` for
/// more details.
pub(crate) fn rcu_read_unlock() {
    RCU_THREAD_BLOCK.with(|thread_block| {
        let nesting_level = thread_block.nesting_level.get();
        if nesting_level > 1 {
            // If the nesting level is greater than 1, this is not the outermost read lock.
            // Decrement the nesting level instead of the read counter.
            thread_block.nesting_level.set(nesting_level - 1);
        } else {
            // This is the outermost read lock. Decrement the read counter.
            let index = thread_block.counter_index.get() as usize;
            let control_block = &RCU_CONTROL_BLOCK;

            #[cfg(feature = "rseq_backend")]
            {
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
                control_block.read_counters.end(index);

                // We cannot tell if this thread is the last thread to exit its read lock, so we
                // always wake the advancer. The advancer will check if there are any active
                // readers and will only advance the state machine if there are no active
                // readers.
                rcu_advancer_wake_all();
            }

            #[cfg(not(feature = "rseq_backend"))]
            {
                // Synchronization point [B] (see design.md)
                let previous_count =
                    control_block.read_counters[index].fetch_sub(1, Ordering::SeqCst);
                if previous_count == 1 {
                    rcu_advancer_wake_all();
                }
            }

            thread_block.nesting_level.set(0);
            thread_block.counter_index.set(u8::MAX);
        }
    });
}

/// Read the value of an RCU pointer.
///
/// This function cannot be called unless the current thread is holding a read lock. The returned
/// pointer is valid until the read lock is released.
pub(crate) fn rcu_read_pointer<T>(ptr: &AtomicPtr<T>) -> *const T {
    // Synchronization point [D] (see design.md)
    ptr.load(Ordering::Acquire)
}

/// Assign a new value to an RCU pointer.
///
/// Concurrent readers may continue to reference the old value of the pointer until the RCU state
/// machine has made sufficient progress. To clean up the old value of the pointer, use `rcu_call`
/// or `rcu_drop`, which defer processing until all in-flight read operations have completed.
pub(crate) fn rcu_assign_pointer<T>(ptr: &AtomicPtr<T>, new_ptr: *mut T) {
    // Synchronization point [E] (see design.md)
    ptr.store(new_ptr, Ordering::Release);
}

/// Replace the value of an RCU pointer.
///
/// Concurrent readers may continue to reference the old value of the pointer until the RCU state
/// machine has made sufficient progress. To clean up the old value of the pointer, use `rcu_call`
/// or `rcu_drop`, which defer processing until all in-flight read operations have completed.
pub(crate) fn rcu_replace_pointer<T>(ptr: &AtomicPtr<T>, new_ptr: *mut T) -> *mut T {
    // Synchronization point [F] (see design.md)
    ptr.swap(new_ptr, Ordering::AcqRel)
}

/// Call a callback to run after all in-flight read operations have completed.
///
/// To wait until the callback is run, call `rcu_synchronize()`. The callback might be called from
/// an arbitrary thread.
pub(crate) fn rcu_call(callback: impl FnOnce() + Send + Sync + 'static) {
    RCU_THREAD_BLOCK.with(|block| {
        block.has_pending_callbacks.set(true);
    });

    // Even though we push the callback to the front of the stack, we reverse the order of the stack
    // when we pop the callbacks from the stack to ensure that the callbacks are run in the order in
    // which they were scheduled.

    // Synchronization point [G] (see design.md)
    RCU_CONTROL_BLOCK.callback_chain.push_front(Box::new(callback));
}

/// Schedule the object to be dropped after all in-flight read operations have completed.
///
/// To wait until the object is dropped, call `rcu_synchronize()`. The object might be dropped from
/// an arbitrary thread.
pub fn rcu_drop<T: Send + Sync + 'static>(value: T) {
    rcu_call(move || {
        std::mem::drop(value);
    });
}

/// Check if there are any active readers for the given generation.
fn has_active_readers(generation: usize) -> bool {
    let index = generation & 1;

    #[cfg(feature = "rseq_backend")]
    {
        return RCU_CONTROL_BLOCK.read_counters.has_active(index);
    }

    #[cfg(not(feature = "rseq_backend"))]
    {
        // Synchronization point [C] (see design.md)
        RCU_CONTROL_BLOCK.read_counters[index].load(Ordering::SeqCst) > 0
    }
}

/// Wake up all the threads that are waiting to advance the state machine.
///
/// Does nothing if no threads are waiting.
fn rcu_advancer_wake_all() {
    let advancer = &RCU_CONTROL_BLOCK.advancer;
    if advancer.load(Ordering::SeqCst) == ADVANCER_WAITING {
        advancer.store(ADVANCER_IDLE, Ordering::Relaxed);
        advancer.wake_all();
    }
}

/// Blocks the current thread until all in-flight read operations have completed for the given
/// generation.
///
/// Postcondition: The number of active readers for the given generation is zero and the advancer
/// futex contains `ADVANCER_IDLE`.
fn rcu_advancer_wait(generation: usize) {
    let advancer = &RCU_CONTROL_BLOCK.advancer;
    loop {
        // In order to avoid a race with `rcu_advancer_wake_all`, we must store `ADVANCER_WAITING`
        // before checking if there are any active readers.
        //
        // In the single total order, either this store or the last decrement to the reader counter
        // must happen first.
        //
        //  (1) If this store happens first, then the last thread to decrement the reader counter
        //      for this generation will observe `ADVANCER_WAITING` and will reset the value to
        //      `ADVANCER_IDLE` and wake the futex, unblocking this thread.
        //
        //  (2) If the last decrement to the reader counter happens first, then this thread will see
        //      that there are no active readers in this generation and avoid blocking on the futex.
        advancer.store(ADVANCER_WAITING, Ordering::SeqCst);
        if !has_active_readers(generation) {
            break;
        }
        let _ = advancer.wait(ADVANCER_WAITING, None, zx::MonotonicInstant::INFINITE);
    }
    advancer.store(ADVANCER_IDLE, Ordering::SeqCst);
}

/// Advance the RCU state machine.
///
/// This function blocks until all in-flight read operations have completed for the current
/// generation and all callbacks have been run.
fn rcu_grace_period() {
    let callbacks = {
        let mut waiting_callbacks = RCU_CONTROL_BLOCK.waiting_callbacks.lock();
        // We are in the *Idle* state.

        // Synchronization point [H] (see design.md)
        let callbacks = RCU_CONTROL_BLOCK.callback_chain.drain();
        let generation = RCU_CONTROL_BLOCK.generation.fetch_add(1, Ordering::Relaxed);

        waiting_callbacks.enqueue(generation, callbacks);

        // Enter the *Waiting* state.
        rcu_advancer_wait(generation);
        waiting_callbacks.take_ready(generation)

        // Return to the *Idle* state.
    };

    // Run the callbacks in reverse order to ensure that the callbacks are run in the order in which
    // they were scheduled.
    for callback in callbacks.into_iter().rev() {
        callback();
    }
}

/// Block until all in-flight read operations and callbacks have completed.
pub fn rcu_synchronize() {
    RCU_THREAD_BLOCK.with(|block| {
        assert!(!block.holding_read_lock());
        block.has_pending_callbacks.set(false);
    });
    for _ in 0..QUEUE_LENGTH {
        rcu_grace_period();
    }
}

/// Run all callbacks that have been scheduled from this thread.
///
/// If any callbacks have been scheduled from this thread, this function will block until all
/// callbacks have been run. If no callbacks have been scheduled from this thread, this function
/// will return immediately.
pub fn rcu_run_callbacks() {
    RCU_THREAD_BLOCK.with(|block| {
        assert!(!block.holding_read_lock());
        if block.has_pending_callbacks.get() {
            rcu_synchronize();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_rcu_delay_regression() {
        // This test relies on the global RCU state machine.
        // It verifies that callbacks are NOT executed immediately after one grace period.

        let flag = Arc::new(AtomicBool::new(false));
        let moved_flag = flag.clone();

        rcu_call(move || {
            moved_flag.store(true, Ordering::SeqCst);
        });

        for _ in 0..QUEUE_LENGTH - 1 {
            rcu_grace_period();

            assert!(
                !flag.load(Ordering::SeqCst),
                "Callback executed too early! RCU requires QUEUE_LENGTH grace periods delay."
            );
        }

        rcu_grace_period();
        assert!(
            flag.load(Ordering::SeqCst),
            "Callback should have executed after QUEUE_LENGTH grace periods"
        );
    }

    #[test]
    fn test_rcu_synchronize() {
        // This test relies on the global RCU state machine.
        // It verifies that rcu_synchronize() blocks until all callbacks have been run.

        let flag = Arc::new(AtomicBool::new(false));
        let moved_flag = flag.clone();

        rcu_call(move || {
            moved_flag.store(true, Ordering::SeqCst);
        });

        rcu_synchronize();
        assert!(
            flag.load(Ordering::SeqCst),
            "Callback should have executed after rcu_synchronize()"
        );
    }

    #[test]
    fn test_rcu_run_callbacks() {
        // This test relies on the global RCU state machine.
        // It verifies that rcu_run_callbacks() blocks until all callbacks have been run.

        let flag = Arc::new(AtomicBool::new(false));
        let moved_flag = flag.clone();

        rcu_call(move || {
            moved_flag.store(true, Ordering::SeqCst);
        });

        rcu_run_callbacks();
        assert!(
            flag.load(Ordering::SeqCst),
            "Callback should have executed after rcu_run_callbacks()"
        );
    }
}
