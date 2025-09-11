// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic_stack::AtomicStack;
use fuchsia_sync::Mutex;
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::thread_local;

type RcuCallback = Box<dyn FnOnce() + Send + Sync + 'static>;

/// The mode of the RCU state machine.
///
/// See `design.md` for more details.
#[derive(Debug, PartialEq, Eq)]
enum RcuMode {
    /// The initial state for the RCU state machine.
    ///
    /// In this state, readers can begin read operations.
    Idle,

    /// The state for the RCU state machine when waiting for callbacks to be run.
    ///
    /// In this state, existing readers complete and decrement their `read_counter`.
    /// New readers can begin read operations and increment a different `read_counter`.
    Waiting,
}

/// The length of the queue of waiting callbacks.
///
/// The state machine waits for this many generations to complete before running these callbacks.
const QUEUE_LENGTH: usize = 2;

/// The queue of waiting callbacks.
///
/// The queue is a ring buffer of sets of callbacks of length `QUEUE_LENGTH`.
struct CallbackQueue {
    /// The index at which to add the next set of callbacks.
    index: usize,

    /// The callbacks that are waiting to be run.
    ///
    /// The callbacks are stored in a ring buffer.
    callbacks: [Vec<RcuCallback>; QUEUE_LENGTH],
}

impl CallbackQueue {
    /// Create an empty callback queue.
    const fn new() -> Self {
        Self { index: 0, callbacks: [Vec::new(), Vec::new()] }
    }

    /// Add a set of callbacks to the back of the queue.
    ///
    /// The caller is responsible for ensuring that there is an empty slot in the ring buffer to
    /// store the callbacks.
    fn push_back(&mut self, callbacks: Vec<RcuCallback>) {
        assert!(self.callbacks[self.index].is_empty());
        self.callbacks[self.index] = callbacks;
        self.index = (self.index + 1) % QUEUE_LENGTH;
    }

    /// Pop the front set of callbacks from the queue.
    ///
    /// If the queue is empty, this function returns an empty vector.
    fn pop_front(&mut self) -> Vec<RcuCallback> {
        self.index = (self.index + 1) % QUEUE_LENGTH;
        std::mem::take(&mut self.callbacks[self.index])
    }
}

/// The mutable state of the RCU state machine.
///
/// This state is used to track the mode of the state machine and the queue of waiting callbacks.
struct RcuMutableState {
    /// The mode of the RCU state machine.
    ///
    /// The mode is used to track the state of the state machine.
    mode: RcuMode,

    /// The queue of waiting callbacks.
    ///
    /// Callbacks are added to this queue when the state machine leaves the `Idle` state. They are
    /// run when the state machine leaves the `Waiting` state after `QUEUE_LENGTH` generations
    /// have completed.
    waiting_callbacks: CallbackQueue,
}

impl RcuMutableState {
    /// Create a new mutable state.
    ///
    /// The state machine starts in the `Idle` state.
    const fn new() -> Self {
        Self { mode: RcuMode::Idle, waiting_callbacks: CallbackQueue::new() }
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
    read_counters: [AtomicUsize; 2],

    /// The chain of callbacks that are waiting to be run.
    ///
    /// Writers add callbacks to this chain after writing to the object. The callbacks are run when
    /// all currently in-flight read operations have completed.
    callback_chain: AtomicStack<RcuCallback>,

    /// The mutable state of the RCU state machine.
    ///
    /// This state is used to track the mode of the state machine and the queue of waiting callbacks.
    mutable_state: Mutex<RcuMutableState>,
}

impl RcuControlBlock {
    /// Create a new control block for the RCU state machine.
    const fn new() -> Self {
        Self {
            generation: AtomicUsize::new(0),
            read_counters: [AtomicUsize::new(0), AtomicUsize::new(0)],
            callback_chain: AtomicStack::new(),
            mutable_state: Mutex::new(RcuMutableState::new()),
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
}

thread_local! {
    /// Thread-specific data for the RCU state machine.
    ///
    /// This data is used to track the nesting level of read locks and the index of the read counter
    /// that the thread incremented when it entered its outermost read lock.
    static RCU_THREAD_BLOCK: RcuThreadBlock = RcuThreadBlock::default();
}

/// Check if the current thread is holding a read lock.
fn rcu_holding_read_lock() -> bool {
    RCU_THREAD_BLOCK.with(|block| block.nesting_level.get() > 0)
}

/// Acquire a read lock.
///
/// This function is used to acquire a read lock on the RCU state machine. The RCU state machine
/// defers calling callbacks until all currently in-flight read operations have completed.
///
/// Must be balanced by a call to `rcu_read_unlock` on the same thread.
pub(crate) fn rcu_read_lock() {
    RCU_THREAD_BLOCK.with(|block| {
        let nesting_level = block.nesting_level.get();
        if nesting_level > 0 {
            // If this thread already has a read lock, increment the nesting level instead of the
            // incrementing the read counter. This approach is a performance optimization to reduce
            // the number of atomic operations that need to be performed.
            block.nesting_level.set(nesting_level + 1);
        } else {
            // This is the outermost read lock. Increment the read counter.
            let index = RCU_CONTROL_BLOCK.generation.load(Ordering::Relaxed) & 1;
            // Synchronization point [A] (see design.md)
            RCU_CONTROL_BLOCK.read_counters[index].fetch_add(1, Ordering::SeqCst);
            block.counter_index.set(index as u8);
            block.nesting_level.set(1);
        }
    });
}

/// Release a read lock.
///
/// This function is used to release a read lock on the RCU state machine. See `rcu_read_lock` for
/// more details.
pub(crate) fn rcu_read_unlock() {
    RCU_THREAD_BLOCK.with(|block| {
        let nesting_level = block.nesting_level.get();
        if nesting_level > 1 {
            // If the nesting level is greater than 1, this is not the outermost read lock.
            // Decrement the nesting level instead of the read counter.
            block.nesting_level.set(nesting_level - 1);
        } else {
            // This is the outermost read lock. Decrement the read counter.
            let index = block.counter_index.get() as usize;
            // Synchronization point [B] (see design.md)
            RCU_CONTROL_BLOCK.read_counters[index].fetch_sub(1, Ordering::Release);
            block.nesting_level.set(0);
            block.counter_index.set(u8::MAX);
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
    // Even though we push the callback to the front of the stack, we reverse the order of the stack
    // when we pop the callbacks from the stack to ensure that the callbacks are run in the order in
    // which they were scheduled.

    // Synchronization point [G] (see design.md)
    RCU_CONTROL_BLOCK.callback_chain.push_front(Box::new(callback));
}

/// Schedule the object ot be dropped after all in-flight read operations have completed.
///
/// To wait until the object is dropped, call `rcu_synchronize()`. The object might be dropped from
/// an arbitrary thread.
pub(crate) fn rcu_drop<T: Send + Sync + 'static>(value: T) {
    rcu_call(move || {
        std::mem::drop(value);
    });
}

/// Attempt to advance the RCU state machine.
///
/// The function returns the current generation number after attempting to advance the state
/// machine, which might be the same as the generation number before the attempt.
pub(crate) fn rcu_try_advance_state() -> usize {
    fn step() -> (usize, Vec<RcuCallback>) {
        let mut state = RCU_CONTROL_BLOCK.mutable_state.lock();
        loop {
            match state.mode {
                RcuMode::Idle => {
                    // Synchronization point [H] (see design.md)
                    let callbacks = RCU_CONTROL_BLOCK.callback_chain.drain();
                    RCU_CONTROL_BLOCK.generation.fetch_add(1, Ordering::Relaxed);
                    state.waiting_callbacks.push_back(callbacks);
                    state.mode = RcuMode::Waiting;
                }
                RcuMode::Waiting => {
                    let current_generation = RCU_CONTROL_BLOCK.generation.load(Ordering::Relaxed);
                    let previous_generation = current_generation - 1;
                    let i = previous_generation & 1;
                    // Synchronization point [C] (see design.md)
                    let pending_reader_count =
                        RCU_CONTROL_BLOCK.read_counters[i].load(Ordering::SeqCst);
                    if pending_reader_count == 0 {
                        let callbacks = state.waiting_callbacks.pop_front();
                        state.mode = RcuMode::Idle;
                        return (current_generation, callbacks);
                    }
                    return (current_generation, vec![]);
                }
            }
        }
    }

    let (generation, callbacks) = step();

    // Run the callbacks in reverse order to ensure that the callbacks are run in the order in which
    // they were scheduled.
    for callback in callbacks.into_iter().rev() {
        callback();
    }

    generation
}

/// Block until all in-flight read operations and callbacks have completed.
///
/// TODO: Currently, this function spins hot, which is not ideal. We should add a futex to wait for
/// this condition more efficiently.
pub(crate) fn rcu_synchronize() {
    assert!(!rcu_holding_read_lock());
    let mut generation = rcu_try_advance_state();

    // We need to wait for `QUEUE_LENGTH` generations to complete to ensure that all the callbacks
    // that were scheduled before this function was called have been run.
    let target_generation = generation.wrapping_add(QUEUE_LENGTH);
    while generation != target_generation {
        // TODO: Block on a futex until it's likely that we can advance the state machine.
        generation = rcu_try_advance_state();
    }
}
