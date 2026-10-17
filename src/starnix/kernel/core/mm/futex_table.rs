// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::memory::MemoryObject;
use crate::mm::{CompareExchangeResult, MemoryManager, ProtectionFlags};
use crate::task::{CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, Task, Waiter};
use futures::channel::oneshot;
use rustc_hash::FxHasher;
use starnix_sync::{FutexTableStateLock, InterruptibleEvent, LockDepMutex, ordered_lock};
use starnix_types::futex_address::FutexAddress;
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{FUTEX_BITSET_MATCH_ANY, FUTEX_TID_MASK, FUTEX_WAITERS, errno, error};
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::marker::PhantomPinned;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Weak};

/// Number of hash buckets in the kernel-wide [`FutexTable`]. Must be a power of two.
const NUM_BUCKETS: usize = 1024;

/// Compact 16-byte key identifying either a private (`FUTEX_PRIVATE_FLAG`) or shared futex.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct FutexKeyValue {
    /// For private futexes: `(mm_ptr as u64) | 1` (low bit 1).
    /// For shared futexes: `koid.raw_koid() << 1` (low bit 0).
    id_and_tag: u64,
    /// For private futexes: `addr.ptr() as u64`.
    /// For shared futexes: `offset`.
    offset: u64,
}

impl FutexKeyValue {
    #[inline]
    fn private(mm: &Arc<MemoryManager>, addr: FutexAddress) -> Self {
        // `Arc::as_ptr(mm)` uniquely identifies the process address space (analogous to Linux's
        // `struct mm_struct *mm` in `union futex_key::private`). Every waiting thread holds its
        // `Arc<MemoryManager>` alive for the entire duration of the wait and unlinks its
        // `WaiterNode` via `StackWaiterGuard` before returning, so `mm` cannot be freed or have
        // its address reused while any waiter with this key is queued.
        let mm_ptr = Arc::as_ptr(mm) as usize as u64;
        debug_assert_eq!(mm_ptr & 1, 0);
        Self { id_and_tag: mm_ptr | 1, offset: addr.ptr() as u64 }
    }

    #[inline]
    fn shared(memory: &MemoryObject, offset: u64) -> Self {
        // No chance of collisions since koids are never reused:
        // https://fuchsia.dev/fuchsia-src/concepts/kernel/concepts#kernel_object_ids
        Self { id_and_tag: memory.get_koid().raw_koid() << 1, offset }
    }
}

/// A kernel-wide table of futexes sharded across `NUM_BUCKETS` hash buckets.
///
/// Each 32-bit aligned address in an address space can potentially have an associated futex that
/// userspace can wait upon. Both private (`FUTEX_PRIVATE_FLAG`) and shared futexes share this
/// table via [`FutexKeyValue`]. Each bucket holds intrusive doubly-linked lists of
/// stack-allocated waiter nodes for internal waiters (`wait`, `wait_boot`, and `lock_pi`),
/// eliminating per-process table allocations and per-wait heap allocations.
pub struct FutexTable {
    buckets: Box<[LockDepMutex<FutexBucket, FutexTableStateLock>; NUM_BUCKETS]>,
}

impl Default for FutexTable {
    fn default() -> Self {
        let buckets: Vec<LockDepMutex<FutexBucket, FutexTableStateLock>> =
            std::iter::repeat_with(|| LockDepMutex::new(FutexBucket::default()))
                .take(NUM_BUCKETS)
                .collect();
        Self { buckets: buckets.into_boxed_slice().try_into().unwrap_or_else(|_| unreachable!()) }
    }
}

impl FutexTable {
    #[inline]
    fn bucket_index(key: &FutexKeyValue) -> usize {
        let mut hasher = FxHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() as usize) & (NUM_BUCKETS - 1)
    }

    /// Wait on the futex at the given address given a boot deadline.
    ///
    /// See FUTEX_WAIT when passed a deadline in CLOCK_REALTIME.
    pub fn wait_boot<Key: FutexKey>(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        value: u32,
        mask: u32,
        deadline: zx::BootInstant,
        timer_slack: zx::BootDuration,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mm = current_task.mm()?;
        // Pre-fault the address before acquiring the bucket lock to make page faults or memory
        // reclamation under the lock highly unlikely.
        // TODO(https://fxbug.dev/548025031): Implement a non-blocking try_load and retry loop once
        // supported by Zircon.
        let _ = mm.atomic_load_u32_acquire(addr)?;
        let key = Key::get(&mm, addr)?;
        let b = Self::bucket_index(&key);

        let node = WaiterNode::new_stack_boot(key, mask, b as u16);
        let timer = zx::BootTimer::create();
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::None,
            event_handler: EventHandler::None,
            err_code: Some(errno!(ETIMEDOUT)),
        };
        node.waiter()
            .wake_on_zircon_signals(&timer, zx::Signals::TIMER_SIGNALED, signal_handler)
            .expect("wait can only fail in OOM conditions");
        timer
            .set(deadline, timer_slack)
            .expect("timer set cannot fail with valid handles and slack");

        let mut bucket = self.buckets[b].lock();
        // As the bucket is locked, no wake can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex wake.
        // Acquire ordering to synchronize with userspace modifications to the value on other
        // threads.
        let loaded_value = mm.atomic_load_u32_acquire(addr)?;
        if value != loaded_value {
            return error!(EAGAIN);
        }
        // Linux futex_wait(): an already-expired timer returns ETIMEDOUT without sleeping.
        if deadline != zx::BootInstant::INFINITE && deadline <= zx::BootInstant::get() {
            return error!(ETIMEDOUT);
        }

        let queued_guard = StackWaiterGuard::insert_waiter(self, &mut bucket, &node);
        std::mem::drop(bucket);

        let result = node.waiter().wait(current_task);
        queued_guard.resolve(result)
    }

    /// Wait on the futex at the given address.
    ///
    /// See FUTEX_WAIT.
    pub fn wait<Key: FutexKey>(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        value: u32,
        mask: u32,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mm = current_task.mm()?;
        // Pre-fault the address before acquiring the bucket lock to make page faults or memory
        // reclamation under the lock highly unlikely.
        // TODO(https://fxbug.dev/548025031): Implement a non-blocking try_load and retry loop once
        // supported by Zircon.
        let _ = mm.atomic_load_u32_acquire(addr)?;

        let key = Key::get(&mm, addr)?;
        let b = Self::bucket_index(&key);
        let node = WaiterNode::new_stack_event(key, mask, 0, b as u16);
        let event_guard = node.event().begin_wait();

        let mut bucket = self.buckets[b].lock();
        // As the bucket is locked, no wake can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex wake.
        // Acquire ordering to synchronize with userspace modifications to the value on other
        // threads.
        let loaded_value = mm.atomic_load_u32_acquire(addr)?;
        if value != loaded_value {
            return error!(EAGAIN);
        }
        // Linux futex_wait(): an already-expired timer returns ETIMEDOUT without sleeping.
        if deadline != zx::MonotonicInstant::INFINITE && deadline <= zx::MonotonicInstant::get() {
            return error!(ETIMEDOUT);
        }

        let queued_guard = StackWaiterGuard::insert_waiter(self, &mut bucket, &node);
        std::mem::drop(bucket);

        let result = current_task.block_until(event_guard, deadline);
        queued_guard.resolve(result)
    }

    /// Wake the given number of waiters on futex at the given address. Returns the number of
    /// waiters actually woken.
    ///
    /// See FUTEX_WAKE.
    pub fn wake<Key: FutexKey>(
        &self,
        task: &Task,
        addr: UserAddress,
        count: usize,
        mask: u32,
    ) -> Result<usize, Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mm = task.mm()?;
        let key = Key::get(&mm, addr)?;
        let b = Self::bucket_index(&key);
        let woken = self.buckets[b].lock().wake(&key, count, mask);
        Ok(woken)
    }

    /// Requeue the waiters to another address.
    ///
    /// See FUTEX_CMP_REQUEUE
    pub fn requeue<Key: FutexKey>(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        wake_count: usize,
        requeue_count: usize,
        new_addr: UserAddress,
        expected_value: Option<u32>,
    ) -> Result<usize, Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let new_addr = FutexAddress::try_from(new_addr)?;
        let mm = current_task.mm()?;
        if expected_value.is_some() {
            // Pre-fault the address before acquiring the bucket lock to make page faults or
            // memory reclamation under the lock highly unlikely.
            // TODO(https://fxbug.dev/548025031): Implement a non-blocking try_load and retry loop
            // once supported by Zircon.
            let _ = mm.atomic_load_u32_acquire(addr)?;
        }
        let key = Key::get(&mm, addr)?;
        let new_key = Key::get(&mm, new_addr)?;
        self.requeue_by_key(key, new_key, wake_count, requeue_count, || {
            if let Some(expected) = expected_value {
                // Use acquire ordering here to synchronize with mutex impls that store w/
                // release ordering.
                let value = mm.atomic_load_u32_acquire(addr)?;
                if value != expected {
                    return error!(EAGAIN);
                }
            }
            Ok(())
        })
    }

    fn requeue_by_key(
        &self,
        key: FutexKeyValue,
        new_key: FutexKeyValue,
        wake_count: usize,
        requeue_count: usize,
        check_expected: impl FnOnce() -> Result<(), Errno>,
    ) -> Result<usize, Errno> {
        let b1 = Self::bucket_index(&key);
        let b2 = Self::bucket_index(&new_key);
        if b1 == b2 {
            let mut bucket = self.buckets[b1].lock();
            check_expected()?;
            // Wake up at most `wake_count` waiters.
            let woken = bucket.wake(&key, wake_count, FUTEX_BITSET_MATCH_ANY);
            // Dequeue up to `requeue_count` waiters and requeue onto `new_key`.
            let (moved, count) = bucket.take_for_requeue(&key, requeue_count);
            bucket.requeue_from(moved, new_key, b1 as u16);
            Ok(woken + count)
        } else {
            let (mut bucket1, mut bucket2) = ordered_lock(&self.buckets[b1], &self.buckets[b2]);
            check_expected()?;
            // Wake up at most `wake_count` waiters.
            let woken = bucket1.wake(&key, wake_count, FUTEX_BITSET_MATCH_ANY);
            // Dequeue up to `requeue_count` waiters and requeue onto `bucket2`.
            let (moved, count) = bucket1.take_for_requeue(&key, requeue_count);
            bucket2.requeue_from(moved, new_key, b2 as u16);
            Ok(woken + count)
        }
    }

    /// Lock the futex at the given address.
    ///
    /// See FUTEX_LOCK_PI.
    pub fn lock_pi<Key: FutexKey>(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mm = current_task.mm()?;
        // Perform a placeholder CAS to pre-fault the page with write permissions (e.g. for COW
        // pages) before acquiring the bucket lock to make page faults or memory reclamation under
        // the lock highly unlikely.
        // TODO(https://fxbug.dev/548025031): Implement a non-blocking try_load and retry loop once
        // supported by Zircon.
        if let Ok(current) = mm.atomic_load_u32_relaxed(addr) {
            let _ = mm.atomic_compare_exchange_u32_acq_rel(addr, current, current);
        }
        let tid = current_task.get_tid() as u32;
        let key = Key::get(&mm, addr)?;
        let b = Self::bucket_index(&key);
        let node = WaiterNode::new_stack_event(key, u32::MAX, tid, b as u16);
        let event_guard = node.event().begin_wait();

        let mut bucket = self.buckets[b].lock();
        // As the bucket is locked, no unlock can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex unlock.
        // Use a relaxed ordering because the compare/exchange below creates a synchronization
        // point with userspace threads in the success case. No synchronization is required in
        // failure cases.
        let mut current_value = mm.atomic_load_u32_relaxed(addr)?;
        let new_owner_tid = loop {
            let new_owner_tid = current_value & FUTEX_TID_MASK;
            if new_owner_tid == tid {
                // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
                //
                //   EDEADLK
                //          (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
                //          FUTEX_CMP_REQUEUE_PI) The futex word at uaddr is
                //          already locked by the caller.
                return error!(EDEADLOCK);
            }

            if current_value == 0 {
                // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops
                // and with the release ordering on userspace unlock ops.
                match mm.atomic_compare_exchange_weak_u32_acq_rel(addr, current_value, tid) {
                    CompareExchangeResult::Success => return Ok(()),
                    CompareExchangeResult::Stale { observed } => {
                        current_value = observed;
                        continue;
                    }
                    CompareExchangeResult::Error(e) => return Err(e),
                }
            }

            // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops and
            // with the release ordering on userspace unlock ops.
            let target_value = current_value | FUTEX_WAITERS;
            match mm.atomic_compare_exchange_u32_acq_rel(addr, current_value, target_value) {
                CompareExchangeResult::Success => (),
                CompareExchangeResult::Stale { observed } => {
                    current_value = observed;
                    continue;
                }
                CompareExchangeResult::Error(e) => return Err(e),
            }
            break new_owner_tid;
        };

        let queued_guard = StackWaiterGuard::insert_rt_mutex(self, &mut bucket, &node);
        std::mem::drop(bucket);

        // ESRCH  (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
        //        FUTEX_CMP_REQUEUE_PI) The thread ID in the futex word at
        //        uaddr does not exist.
        let result = current_task
            .get_task(new_owner_tid as i32)
            .ok()
            .and_then(|o| o.running_state().unwrap().thread.get().map(|t| Arc::clone(&t.thread)))
            .map_or_else(
                || error!(ESRCH),
                |owner| current_task.block_with_owner_until(event_guard, &owner, deadline),
            );

        // Being gone from the queue means `unlock_pi` picked this waiter as the new owner and
        // already published its tid in the futex word, so the mutex is held even though the wait
        // itself failed. Reporting the error would leave the mutex locked by a thread that
        // believes it does not own it.
        queued_guard.resolve(result)
    }

    /// Unlock the futex at the given address.
    ///
    /// See FUTEX_UNLOCK_PI.
    pub fn unlock_pi<Key: FutexKey>(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mm = current_task.mm()?;
        // Perform a placeholder CAS to pre-fault the page with write permissions (e.g. for COW
        // pages) before acquiring the bucket lock to make page faults or memory reclamation under
        // the lock highly unlikely.
        // TODO(https://fxbug.dev/548025031): Implement a non-blocking try_load and retry loop once
        // supported by Zircon.
        if let Ok(current) = mm.atomic_load_u32_relaxed(addr) {
            let _ = mm.atomic_compare_exchange_u32_acq_rel(addr, current, current);
        }
        let tid = current_task.get_tid() as u32;
        let key = Key::get(&mm, addr)?;
        let b = Self::bucket_index(&key);
        let mut bucket = self.buckets[b].lock();

        // Use a relaxed ordering because the compare/exchange below creates a synchronization
        // point with userspace threads in the success case. No synchronization is required in
        // failure cases.
        let expected_value = mm.atomic_load_u32_relaxed(addr)?;
        if expected_value & FUTEX_TID_MASK != tid {
            // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
            //
            //   EPERM  (FUTEX_UNLOCK_PI) The caller does not own the lock
            //          represented by the futex word.
            return error!(EPERM);
        }

        let maybe_node = bucket.pop_rt_mutex_waiter(&key);
        let target_value = maybe_node.as_ref().map_or(0, |w| w.tid());

        // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops and
        // with the release ordering on userspace unlock ops.
        let handoff = mm.atomic_compare_exchange_u32_acq_rel(addr, expected_value, target_value);
        if let Err(e) = handoff_result(handoff) {
            if let Some(node) = maybe_node {
                bucket.unpop_rt_mutex_waiter(node);
            }
            return Err(e);
        }

        if let Some(node) = maybe_node {
            // SAFETY: `node` was popped from `bucket` under the current lock acquisition.
            unsafe {
                bucket.wake_and_mark_woken(node);
            }
        }

        Ok(())
    }

    /// Wait on the futex at the given offset in the memory.
    ///
    /// Returns a receiver that will be signaled when the futex is woken, and an
    /// `Arc<()>` token that must be kept alive by the caller for the duration of the
    /// wait. If the caller drops the token (e.g., if the external client
    /// disconnects), the waiter is marked as stale and will be garbage-collected by the
    /// next futex operation on this bucket.
    ///
    /// See FUTEX_WAIT.
    pub fn external_wait(
        &self,
        memory: MemoryObject,
        offset: u64,
        value: u32,
        mask: u32,
    ) -> Result<(Arc<()>, oneshot::Receiver<()>), Errno> {
        let key = FutexKeyValue::shared(&memory, offset);
        let b = Self::bucket_index(&key);
        let mut bucket = self.buckets[b].lock();
        bucket.prune_stale_external();
        // As the bucket is locked, no wake can happen before the waiter is registered.
        Self::external_check_futex_value(&memory, offset, value)?;

        let token = Arc::new(());
        let (sender, receiver) = oneshot::channel::<()>();
        let waiter =
            UnattachedWaiter::new_external(key, mask, b as u16, Arc::downgrade(&token), sender);
        bucket.waiters.push_back(waiter);
        Ok((token, receiver))
    }

    /// Wake the given number of waiters on futex at the given offset in the memory. Returns the
    /// number of waiters actually woken.
    ///
    /// See FUTEX_WAKE.
    pub fn external_wake(
        &self,
        memory: MemoryObject,
        offset: u64,
        count: usize,
        mask: u32,
    ) -> Result<usize, Errno> {
        let key = FutexKeyValue::shared(&memory, offset);
        let b = Self::bucket_index(&key);
        let woken = self.buckets[b].lock().wake(&key, count, mask);
        Ok(woken)
    }

    pub fn external_requeue(
        &self,
        first_memory: MemoryObject,
        first_offset: u64,
        second_memory: Option<MemoryObject>,
        second_offset: u64,
        wake_count: usize,
        requeue_count: usize,
        expected_value: Option<u32>,
    ) -> Result<usize, Errno> {
        let first_key = FutexKeyValue::shared(&first_memory, first_offset);
        let second_key = match second_memory.as_ref() {
            Some(second_memory) => FutexKeyValue::shared(second_memory, second_offset),
            None => FutexKeyValue::shared(&first_memory, second_offset),
        };
        self.requeue_by_key(first_key, second_key, wake_count, requeue_count, || {
            if let Some(expected) = expected_value {
                // The state being locked is how this is included in the set of atomic changes.
                Self::external_check_futex_value(&first_memory, first_offset, expected)?;
            }
            Ok(())
        })
    }

    fn external_check_futex_value(
        memory: &MemoryObject,
        offset: u64,
        value: u32,
    ) -> Result<(), Errno> {
        let loaded_value = {
            // TODO: This read should be atomic.
            let mut buf = [0u8; 4];
            memory.read(&mut buf, offset).map_err(|_| errno!(EINVAL))?;
            u32::from_ne_bytes(buf)
        };
        if loaded_value != value {
            return error!(EAGAIN);
        }
        Ok(())
    }
}

/// Maps the outcome of the compare-exchange that hands a PI mutex over to its next owner.
fn handoff_result(result: CompareExchangeResult<u32>) -> Result<(), Errno> {
    match result {
        CompareExchangeResult::Success => Ok(()),
        // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
        //
        //   EINVAL (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
        //       FUTEX_UNLOCK_PI) The kernel detected an inconsistency
        //       between the user-space state at uaddr and the kernel
        //       state.  This indicates either state corruption or that the
        //       kernel found a waiter on uaddr which is waiting via
        //       FUTEX_WAIT or FUTEX_WAIT_BITSET.
        CompareExchangeResult::Stale { .. } => error!(EINVAL),
        // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
        //
        //   EACCES No read access to the memory of a futex word.
        CompareExchangeResult::Error(_) => error!(EACCES),
    }
}

pub trait FutexKey: Sized {
    fn get(mm: &Arc<MemoryManager>, addr: FutexAddress) -> Result<FutexKeyValue, Errno>;
}

pub struct PrivateFutexKey;

impl FutexKey for PrivateFutexKey {
    fn get(mm: &Arc<MemoryManager>, addr: FutexAddress) -> Result<FutexKeyValue, Errno> {
        Ok(FutexKeyValue::private(mm, addr))
    }
}

pub struct SharedFutexKey;

impl FutexKey for SharedFutexKey {
    fn get(mm: &Arc<MemoryManager>, addr: FutexAddress) -> Result<FutexKeyValue, Errno> {
        let (memory, offset) = mm.get_mapping_memory(addr.into(), ProtectionFlags::READ)?;
        Ok(FutexKeyValue::shared(&memory, offset))
    }
}

enum WaiterKind {
    /// Stack-allocated `InterruptibleEvent` (used by `wait` and `lock_pi`).
    StackEvent(InterruptibleEvent),
    /// Stack-allocated `Waiter` (used by `wait_boot`).
    StackBoot(Waiter),
    /// Heap-allocated external waiter (`Box<WaiterNode>`), owned by the bucket list.
    External { token: Weak<()>, sender: oneshot::Sender<()> },
}

/// An intrusive waiter node.
///
/// For `wait`, `wait_boot`, and `lock_pi`, this node lives directly on the blocked thread's
/// kernel stack frame and is protected by [`StackWaiterGuard`]. For `external_wait`, it is
/// heap-allocated via `Box` and owned by [`WaiterList`].
struct WaiterNode {
    prev: Cell<Option<NonNull<WaiterNode>>>,
    next: Cell<Option<NonNull<WaiterNode>>>,
    key: Cell<FutexKeyValue>,
    mask: u32,
    attached: Cell<bool>,
    woken: AtomicBool,
    bucket_idx: AtomicU16,
    /// The tid, possibly with the FUTEX_WAITERS bit set.
    tid: Cell<u32>,
    kind: WaiterKind,
    _pin: PhantomPinned,
}

impl WaiterNode {
    fn new_stack_event(key: FutexKeyValue, mask: u32, tid: u32, bucket_idx: u16) -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            key: Cell::new(key),
            mask,
            attached: Cell::new(false),
            woken: AtomicBool::new(false),
            bucket_idx: AtomicU16::new(bucket_idx),
            tid: Cell::new(tid),
            kind: WaiterKind::StackEvent(InterruptibleEvent::default()),
            _pin: PhantomPinned,
        }
    }

    fn new_stack_boot(key: FutexKeyValue, mask: u32, bucket_idx: u16) -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            key: Cell::new(key),
            mask,
            attached: Cell::new(false),
            woken: AtomicBool::new(false),
            bucket_idx: AtomicU16::new(bucket_idx),
            tid: Cell::new(0),
            kind: WaiterKind::StackBoot(Waiter::new()),
            _pin: PhantomPinned,
        }
    }

    fn new_external(
        key: FutexKeyValue,
        mask: u32,
        bucket_idx: u16,
        token: Weak<()>,
        sender: oneshot::Sender<()>,
    ) -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            key: Cell::new(key),
            mask,
            attached: Cell::new(false),
            woken: AtomicBool::new(false),
            bucket_idx: AtomicU16::new(bucket_idx),
            tid: Cell::new(0),
            kind: WaiterKind::External { token, sender },
            _pin: PhantomPinned,
        }
    }

    fn event(&self) -> &InterruptibleEvent {
        match &self.kind {
            WaiterKind::StackEvent(event) => event,
            _ => unreachable!(),
        }
    }

    fn waiter(&self) -> &Waiter {
        match &self.kind {
            WaiterKind::StackBoot(waiter) => waiter,
            _ => unreachable!(),
        }
    }

    fn is_stale_external(&self) -> bool {
        matches!(&self.kind, WaiterKind::External { token, .. } if token.strong_count() == 0)
    }
}

/// Linear (non-`Copy`, non-`Clone`) ownership token representing a valid [`WaiterNode`] that is
/// currently unattached from any [`WaiterList`].
///
/// Consuming `UnattachedWaiter` by value in [`WaiterList::push_back`], [`WaiterList::push_front`],
/// [`FutexBucket::wake_and_mark_woken`], and
/// [`FutexBucket::free_stale_external`] guarantees at compile time that only unattached nodes can
/// be inserted, woken, or freed, and that no node can be inserted or woken twice.
#[must_use = "an UnattachedWaiter must be inserted, woken, or freed"]
struct UnattachedWaiter(NonNull<WaiterNode>);

impl UnattachedWaiter {
    /// Allocates a new heap-backed external [`WaiterNode`] in the unattached state.
    fn new_external(
        key: FutexKeyValue,
        mask: u32,
        bucket_idx: u16,
        token: Weak<()>,
        sender: oneshot::Sender<()>,
    ) -> Self {
        Self(Box::into_non_null(Box::new(WaiterNode::new_external(
            key, mask, bucket_idx, token, sender,
        ))))
    }

    /// Creates an [`UnattachedWaiter`] for a stack-allocated `node`.
    ///
    /// Panics if `node` is already attached.
    ///
    /// # Safety
    /// The caller must ensure that `node` is guarded by a [`StackWaiterGuard`] that unlinks or
    /// confirms wakeup of `node` before `node`'s stack frame exits.
    unsafe fn from_stack(node: &WaiterNode) -> Self {
        assert!(!node.attached.get(), "WaiterNode is already attached");
        Self(NonNull::from(node))
    }

    fn into_raw(self) -> NonNull<WaiterNode> {
        std::mem::ManuallyDrop::new(self).0
    }

    /// Updates `key` and `bucket_idx` while holding the destination [`FutexBucket`] lock.
    fn set_requeued(&mut self, _bucket: &mut FutexBucket, new_key: FutexKeyValue, new_bucket: u16) {
        // SAFETY: `self` is unattached and the caller holds the bucket lock(s).
        unsafe {
            let node = self.0.as_ref();
            node.key.set(new_key);
            node.bucket_idx.store(new_bucket, Ordering::Relaxed);
        }
    }

    fn tid(&self) -> u32 {
        // SAFETY: `self` is an unattached waiter protected by the bucket lock.
        unsafe { self.0.as_ref().tid.get() }
    }

    fn set_tid(&mut self, _bucket: &mut FutexBucket, tid: u32) {
        // SAFETY: `self` is an unattached waiter protected by the bucket lock.
        unsafe {
            self.0.as_ref().tid.set(tid);
        }
    }

    /// Consumes the token when a stack-allocated waiter has been unlinked by its own
    /// [`StackWaiterGuard`].
    fn into_removed_stack_node(self) {
        // Nothing to free: the node lives on the waiting thread's stack.
        let _ = self.into_raw();
    }
}

impl Drop for UnattachedWaiter {
    fn drop(&mut self) {
        debug_assert!(false, "UnattachedWaiter dropped without being consumed");
    }
}

/// Intrusive doubly-linked list of [`WaiterNode`]s within a single [`FutexBucket`].
struct WaiterList {
    head: Option<NonNull<WaiterNode>>,
    tail: Option<NonNull<WaiterNode>>,
}

// SAFETY: Access to `WaiterList` and all linked `WaiterNode`s is synchronized by the enclosing
// `FutexBucket` mutex.
unsafe impl Send for WaiterList {}

impl Default for WaiterList {
    fn default() -> Self {
        Self { head: None, tail: None }
    }
}

impl Drop for WaiterList {
    fn drop(&mut self) {
        // Free any remaining heap-allocated `External` nodes when the table is dropped.
        let mut cur = self.head;
        while let Some(node_ptr) = cur {
            // SAFETY: Nodes in `self` are valid while linked.
            unsafe {
                let is_external = {
                    let node = node_ptr.as_ref();
                    cur = node.next.get();
                    matches!(node.kind, WaiterKind::External { .. })
                };
                debug_assert!(
                    is_external,
                    "stack-allocated WaiterNode must not remain linked when WaiterList is dropped"
                );
                if is_external {
                    drop(Box::from_raw(node_ptr.as_ptr()));
                }
            }
        }
    }
}

impl WaiterList {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        let mut count = 0;
        let mut cur = self.head;
        while let Some(node_ptr) = cur {
            count += 1;
            // SAFETY: Valid while linked under bucket lock.
            cur = unsafe { node_ptr.as_ref().next.get() };
        }
        count
    }

    fn push_back(&mut self, waiter: UnattachedWaiter) {
        let node_ptr = waiter.into_raw();
        // SAFETY: `waiter` is an `UnattachedWaiter` (`prev` and `next` are `None`) and `&mut self`
        // proves the enclosing bucket lock is held.
        unsafe {
            let node = node_ptr.as_ref();
            let was_attached = node.attached.replace(true);
            debug_assert!(!was_attached);
            node.prev.set(self.tail);
            node.next.set(None);
            if let Some(tail) = self.tail {
                tail.as_ref().next.set(Some(node_ptr));
            } else {
                self.head = Some(node_ptr);
            }
            self.tail = Some(node_ptr);
        }
    }

    fn push_front(&mut self, waiter: UnattachedWaiter) {
        let node_ptr = waiter.into_raw();
        // SAFETY: `waiter` is an `UnattachedWaiter` (`prev` and `next` are `None`) and `&mut self`
        // proves the enclosing bucket lock is held.
        unsafe {
            let node = node_ptr.as_ref();
            let was_attached = node.attached.replace(true);
            debug_assert!(!was_attached);
            node.prev.set(None);
            node.next.set(self.head);
            if let Some(head) = self.head {
                head.as_ref().prev.set(Some(node_ptr));
            } else {
                self.tail = Some(node_ptr);
            }
            self.head = Some(node_ptr);
        }
    }

    fn pop_front(&mut self) -> Option<UnattachedWaiter> {
        let head = self.head?;
        // SAFETY: `head` is currently linked in `self`.
        Some(unsafe { self.unlink(head) })
    }

    /// Unlinks `node_ptr` from `self` and returns it as an [`UnattachedWaiter`].
    ///
    /// # Safety
    /// `node_ptr` must be currently linked in `self`.
    unsafe fn unlink(&mut self, node_ptr: NonNull<WaiterNode>) -> UnattachedWaiter {
        // SAFETY: `node_ptr` is currently linked in `self` and protected by the bucket lock.
        unsafe {
            let node = node_ptr.as_ref();
            let was_attached = node.attached.replace(false);
            debug_assert!(was_attached);
            let prev = node.prev.replace(None);
            let next = node.next.replace(None);
            if let Some(p) = prev {
                p.as_ref().next.set(next);
            } else {
                self.head = next;
            }
            if let Some(n) = next {
                n.as_ref().prev.set(prev);
            } else {
                self.tail = prev;
            }
        }
        UnattachedWaiter(node_ptr)
    }
}

#[repr(align(64))]
struct FutexBucket {
    waiters: WaiterList,
    rt_mutex_waiters: WaiterList,
}

impl Default for FutexBucket {
    fn default() -> Self {
        Self { waiters: WaiterList::default(), rt_mutex_waiters: WaiterList::default() }
    }
}

impl FutexBucket {
    /// Notifies an unattached waiter node while holding its enclosing [`FutexBucket`] lock
    /// (proven by `&mut self`).
    ///
    /// For stack-allocated nodes, stores `woken = true` (`Ordering::Release`) as the final
    /// access to the node so [`StackWaiterGuard`] can return on the fast path without locking
    /// the bucket once `woken` is observed.
    ///
    /// # Safety
    /// `waiter` must have been unlinked from `self` under the current lock acquisition (`&mut self`).
    unsafe fn wake_and_mark_woken(&mut self, waiter: UnattachedWaiter) -> bool {
        let node_ptr = waiter.into_raw();
        // SAFETY: `waiter` was unlinked from this bucket under `&mut self`, so we have exclusive
        // wake access to `node_ptr`. While `woken` is still `false`, the waiting thread cannot
        // exit `StackWaiterGuard::unlink_or_confirm_woken` without acquiring the bucket lock that
        // `&mut self` currently holds, so `node_ptr` remains valid throughout `notify()` and the
        // `woken` store.
        unsafe {
            let is_external = matches!(node_ptr.as_ref().kind, WaiterKind::External { .. });
            if is_external {
                let boxed = Box::from_raw(node_ptr.as_ptr());
                if let WaiterKind::External { sender, .. } = boxed.kind {
                    sender.send(()).is_ok()
                } else {
                    unreachable!()
                }
            } else {
                let woken_ptr = &raw const (*node_ptr.as_ptr()).woken;
                {
                    let node = node_ptr.as_ref();
                    match &node.kind {
                        WaiterKind::StackEvent(event) => event.notify(),
                        WaiterKind::StackBoot(waiter) => waiter.notify(),
                        WaiterKind::External { .. } => unreachable!(),
                    }
                }
                (*woken_ptr).store(true, Ordering::Release);
                // As soon as `woken` is set to `true` above, the waiting thread can take the
                // lock-free fast path in `StackWaiterGuard::unlink_or_confirm_woken` and pop its
                // stack frame, freeing and invalidating `node_ptr`. Do not access `node_ptr` or
                // `woken_ptr` after this point.
                true
            }
        }
    }

    /// Frees a stale heap-allocated `External` waiter node.
    ///
    /// # Safety
    /// `waiter` must be an `External` node unlinked from `self` under the current lock acquisition.
    unsafe fn free_stale_external(&mut self, waiter: UnattachedWaiter) {
        let node_ptr = waiter.into_raw();
        // SAFETY: `waiter` is an unlinked heap-allocated `External` node owned by this bucket.
        unsafe {
            debug_assert!(matches!(node_ptr.as_ref().kind, WaiterKind::External { .. }));
            drop(Box::from_raw(node_ptr.as_ptr()));
        }
    }

    fn prune_stale_external(&mut self) {
        let mut cur = self.waiters.head;
        while let Some(node_ptr) = cur {
            // SAFETY: `node_ptr` is linked in `self.waiters` under the bucket lock.
            let is_stale = unsafe {
                let node = node_ptr.as_ref();
                cur = node.next.get();
                node.is_stale_external()
            };
            if is_stale {
                // SAFETY: `node_ptr` is currently linked in `self.waiters`.
                unsafe {
                    let unattached = self.waiters.unlink(node_ptr);
                    self.free_stale_external(unattached);
                }
            }
        }
    }

    fn wake(&mut self, key: &FutexKeyValue, count: usize, mask: u32) -> usize {
        let mut woken = 0;
        let mut cur = self.waiters.head;
        while let Some(node_ptr) = cur {
            if woken >= count {
                break;
            }
            // SAFETY: `node_ptr` is linked in `self.waiters` under the bucket lock. We end the
            // `&WaiterNode` borrow before unlinking, freeing, or marking `node_ptr` woken.
            let (is_stale, matches) = unsafe {
                let node = node_ptr.as_ref();
                cur = node.next.get();
                (node.is_stale_external(), node.key.get() == *key && (node.mask & mask) != 0)
            };

            if is_stale {
                // SAFETY: `node_ptr` is currently linked in `self.waiters`.
                unsafe {
                    let unattached = self.waiters.unlink(node_ptr);
                    self.free_stale_external(unattached);
                }
                continue;
            }

            if matches {
                // SAFETY: `node_ptr` is currently linked in `self.waiters`.
                let did_wake = unsafe {
                    let unattached = self.waiters.unlink(node_ptr);
                    self.wake_and_mark_woken(unattached)
                };
                // The send will fail if the receiver is gone, which means nothing was actually
                // waiting on the futex.
                if did_wake {
                    woken += 1;
                }
            }
        }
        woken
    }

    fn take_for_requeue(
        &mut self,
        key: &FutexKeyValue,
        requeue_count: usize,
    ) -> (WaiterList, usize) {
        let mut out = WaiterList::default();
        let mut moved_count = 0;
        let mut cur = self.waiters.head;
        while let Some(node_ptr) = cur {
            if moved_count >= requeue_count {
                break;
            }
            // SAFETY: `node_ptr` is linked in `self.waiters` under the bucket lock. We end the
            // `&WaiterNode` borrow before unlinking or freeing `node_ptr`.
            let (is_stale, matches) = unsafe {
                let node = node_ptr.as_ref();
                cur = node.next.get();
                (node.is_stale_external(), node.key.get() == *key)
            };

            if is_stale {
                // SAFETY: `node_ptr` is currently linked in `self.waiters`.
                unsafe {
                    let unattached = self.waiters.unlink(node_ptr);
                    self.free_stale_external(unattached);
                }
                continue;
            }

            if matches {
                // SAFETY: `node_ptr` is currently linked in `self.waiters`.
                let unattached = unsafe { self.waiters.unlink(node_ptr) };
                out.push_back(unattached);
                moved_count += 1;
            }
        }
        (out, moved_count)
    }

    fn requeue_from(&mut self, mut moved: WaiterList, new_key: FutexKeyValue, new_bucket_idx: u16) {
        while let Some(mut waiter) = moved.pop_front() {
            waiter.set_requeued(self, new_key, new_bucket_idx);
            self.waiters.push_back(waiter);
        }
    }

    /// Pop the next RT-Mutex for the given address.
    fn pop_rt_mutex_waiter(&mut self, key: &FutexKeyValue) -> Option<UnattachedWaiter> {
        let mut cur = self.rt_mutex_waiters.head;
        let mut popped = None;
        let mut has_more_for_key = false;
        while let Some(node_ptr) = cur {
            // SAFETY: `node_ptr` is linked in `self.rt_mutex_waiters` under the bucket lock.
            let matches = unsafe {
                let node = node_ptr.as_ref();
                cur = node.next.get();
                node.key.get() == *key
            };
            if matches {
                if popped.is_none() {
                    // SAFETY: `node_ptr` is currently linked in `self.rt_mutex_waiters`.
                    popped = Some(unsafe { self.rt_mutex_waiters.unlink(node_ptr) });
                } else {
                    has_more_for_key = true;
                    break;
                }
            }
        }
        if let Some(ref mut waiter) = popped {
            if has_more_for_key {
                let tid = waiter.tid();
                waiter.set_tid(self, tid | FUTEX_WAITERS);
            }
        }
        popped
    }

    /// Puts a waiter popped for a handoff that did not happen back at the head of its queue.
    ///
    /// Clearing `FUTEX_WAITERS` undoes what `pop_rt_mutex_waiter` set, restoring the tid the
    /// waiter was queued with. The waiter must go back, otherwise it would conclude from its
    /// absence that it was made the new owner of a mutex it does not hold.
    fn unpop_rt_mutex_waiter(&mut self, mut waiter: UnattachedWaiter) {
        let tid = waiter.tid();
        waiter.set_tid(self, tid & FUTEX_TID_MASK);
        self.rt_mutex_waiters.push_front(waiter);
    }
}

/// RAII guard ensuring that a stack-allocated [`WaiterNode`] is either unlinked from its
/// [`FutexBucket`] or has completed its wake notification under the bucket lock before the
/// stack frame exits.
struct StackWaiterGuard<'a> {
    table: &'a FutexTable,
    node: &'a WaiterNode,
    is_rt_mutex: bool,
    active: bool,
}

impl<'a> StackWaiterGuard<'a> {
    fn insert_waiter(
        table: &'a FutexTable,
        bucket: &mut FutexBucket,
        node: &'a WaiterNode,
    ) -> Self {
        // SAFETY: The returned `StackWaiterGuard` ensures `node` is unlinked or woken before `'a`
        // ends.
        let unattached = unsafe { UnattachedWaiter::from_stack(node) };
        bucket.waiters.push_back(unattached);
        Self { table, node, is_rt_mutex: false, active: true }
    }

    fn insert_rt_mutex(
        table: &'a FutexTable,
        bucket: &mut FutexBucket,
        node: &'a WaiterNode,
    ) -> Self {
        // SAFETY: The returned `StackWaiterGuard` ensures `node` is unlinked or woken before `'a`
        // ends.
        let unattached = unsafe { UnattachedWaiter::from_stack(node) };
        bucket.rt_mutex_waiters.push_back(unattached);
        Self { table, node, is_rt_mutex: true, active: true }
    }

    /// Ensures `self.node` is no longer linked in any bucket and no waker is accessing it.
    ///
    /// Returns `true` if `self.node` was still queued (and was removed by this call), or `false`
    /// if a waker already dequeued and notified `self.node`.
    fn unlink_or_confirm_woken(&self) -> bool {
        // Fast path: if the waker already finished `notify()` and stored `woken = true`
        // (`Ordering::Release`), that store was the waker's final access to `self.node`, so we
        // can return immediately without locking the bucket.
        if self.node.woken.load(Ordering::Acquire) {
            return false;
        }

        // Slow path: either the wait timed out / was interrupted while still queued, or we woke
        // up before the waker finished storing `woken = true` under the bucket lock. Acquiring
        // the bucket mutex serializes with the waker (with Zircon Priority Inheritance if the
        // waker was preempted).
        loop {
            let b = self.node.bucket_idx.load(Ordering::Relaxed) as usize;
            let mut bucket = self.table.buckets[b].lock();
            if self.node.bucket_idx.load(Ordering::Relaxed) as usize != b {
                // `requeue` moved `self.node` to another bucket before we locked `buckets[b]`.
                continue;
            }

            if self.node.woken.load(Ordering::Acquire) {
                return false;
            }

            let list =
                if self.is_rt_mutex { &mut bucket.rt_mutex_waiters } else { &mut bucket.waiters };
            // SAFETY: `!woken` and `bucket_idx == b` under `buckets[b]` lock guarantee that
            // `self.node` is currently linked in `list`.
            let unattached = unsafe { list.unlink(NonNull::from(self.node)) };
            unattached.into_removed_stack_node();
            return true;
        }
    }

    /// Resolves the outcome of a blocking wait on this table.
    ///
    /// On success, the waker has already removed the waiter from the queue. On error (e.g.
    /// ETIMEDOUT, EINTR), this unlinks the waiter from the queue if it is still queued.
    ///
    /// Finding that the waiter was already dequeued and woken means a concurrent wake dequeued
    /// the waiter while it was waking up for another reason. That wake counted this waiter as
    /// woken, so report success rather than dropping the wake on the floor.
    fn resolve(mut self, result: Result<(), Errno>) -> Result<(), Errno> {
        self.active = false;
        let was_still_queued = self.unlink_or_confirm_woken();
        match result {
            Ok(()) => {
                debug_assert!(!was_still_queued);
                Ok(())
            }
            Err(e) => {
                if was_still_queued {
                    Err(e)
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl Drop for StackWaiterGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.unlink_or_confirm_woken();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_uapi::restricted_aspace::RESTRICTED_ASPACE_BASE;
    use starnix_uapi::user_address::UserAddress;

    fn test_key(offset: usize) -> FutexKeyValue {
        let addr =
            FutexAddress::try_from(UserAddress::from((RESTRICTED_ASPACE_BASE + offset) as u64))
                .unwrap();
        FutexKeyValue { id_and_tag: 1, offset: addr.ptr() as u64 }
    }

    fn wake_key(table: &FutexTable, key: &FutexKeyValue, count: usize, mask: u32) -> usize {
        let b = FutexTable::bucket_index(key);
        table.buckets[b].lock().wake(key, count, mask)
    }

    #[fuchsia::test]
    fn test_remove_waiter_simple() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);
        let node = WaiterNode::new_stack_event(key, u32::MAX, 0, b as u16);

        let guard = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b].lock(), &node);
        assert_eq!(table.buckets[b].lock().waiters.len(), 1);

        assert_eq!(guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert!(table.buckets[b].lock().waiters.is_empty());
    }

    #[fuchsia::test]
    fn test_remove_waiter_requeued() {
        let table = FutexTable::default();
        let key1 = test_key(0x1000);
        let b1 = FutexTable::bucket_index(&key1);
        // Find a key2 that hashes to the same bucket as key1.
        let key2 = (2..4096)
            .map(|i| test_key(i * 0x1000))
            .find(|k| FutexTable::bucket_index(k) == b1)
            .unwrap();

        let node = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
        let guard = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b1].lock(), &node);

        let requeued = table.requeue_by_key(key1, key2, 0, 1, || Ok(())).unwrap();
        assert_eq!(requeued, 1);
        assert_eq!(table.buckets[b1].lock().waiters.len(), 1);

        assert_eq!(guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert!(table.buckets[b1].lock().waiters.is_empty());
    }

    #[fuchsia::test]
    fn test_remove_waiter_requeued_across_buckets() {
        let table = FutexTable::default();
        let key1 = test_key(0x1000);
        // Find a key2 that hashes to a different bucket than key1.
        let b1 = FutexTable::bucket_index(&key1);
        let key2 = (2..1024)
            .map(|i| test_key(i * 0x1000))
            .find(|k| FutexTable::bucket_index(k) != b1)
            .unwrap();
        let b2 = FutexTable::bucket_index(&key2);

        let node = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
        let guard = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b1].lock(), &node);

        let requeued = table.requeue_by_key(key1, key2, 0, 1, || Ok(())).unwrap();
        assert_eq!(requeued, 1);
        assert!(table.buckets[b1].lock().waiters.is_empty());
        assert_eq!(table.buckets[b2].lock().waiters.len(), 1);

        assert_eq!(guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert!(table.buckets[b2].lock().waiters.is_empty());
    }

    /// A wake that races with an interruption must not be lost.
    ///
    /// When a signal interrupts a waiter, the waiter can still be sitting in the queue when
    /// another thread issues a wake. That wake counts the waiter as woken, so the waiter must
    /// notice that it has been dequeued and report the wake instead of the interruption.
    #[fuchsia::test]
    fn test_wake_racing_with_interruption_is_not_lost() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);
        let node = WaiterNode::new_stack_event(key, u32::MAX, 0, b as u16);
        let _event_guard = node.event().begin_wait();

        let guard = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b].lock(), &node);

        // A signal interrupts the waiter before it is dequeued.
        node.event().interrupt();

        // The waker still sees the waiter in the queue and counts it as woken.
        assert_eq!(wake_key(&table, &key, 1, FUTEX_BITSET_MATCH_ANY), 1);

        // The interrupted waiter must therefore observe that it is no longer queued, which tells
        // it to report the wake rather than the interruption.
        assert_eq!(guard.resolve(error!(EINTR)), Ok(()));
    }

    /// The boot-deadline flavor of `FUTEX_WAIT` must resolve the same race the same way.
    #[fuchsia::test]
    fn test_wake_racing_with_boot_waiter_removal_is_not_lost() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);
        let node = WaiterNode::new_stack_boot(key, u32::MAX, b as u16);

        let guard = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b].lock(), &node);

        // The waker dequeues the waiter and counts it as woken, whether or not the waiter was
        // about to give up on its deadline.
        assert_eq!(wake_key(&table, &key, 1, FUTEX_BITSET_MATCH_ANY), 1);
        assert_eq!(guard.resolve(error!(ETIMEDOUT)), Ok(()));
    }

    /// Removing one waiter on a key must leave other waiters (including different waiter kinds)
    /// queued and wakeable.
    #[fuchsia::test]
    fn test_remove_waiter_ignores_other_waiter_kinds() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);

        let event_node = WaiterNode::new_stack_event(key, u32::MAX, 0, b as u16);
        let boot_node = WaiterNode::new_stack_boot(key, u32::MAX, b as u16);

        let (event_guard, boot_guard) = {
            let mut bucket = table.buckets[b].lock();
            (
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &event_node),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &boot_node),
            )
        };
        assert_eq!(table.buckets[b].lock().waiters.len(), 2);

        assert_eq!(event_guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert_eq!(table.buckets[b].lock().waiters.len(), 1);

        assert_eq!(wake_key(&table, &key, 1, FUTEX_BITSET_MATCH_ANY), 1);
        assert_eq!(boot_guard.resolve(error!(ETIMEDOUT)), Ok(()));
        assert!(table.buckets[b].lock().waiters.is_empty());
    }

    #[fuchsia::test]
    fn test_remove_rt_mutex_waiter() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);
        let node = WaiterNode::new_stack_event(key, u32::MAX, 1, b as u16);

        let guard = StackWaiterGuard::insert_rt_mutex(&table, &mut table.buckets[b].lock(), &node);
        assert_eq!(table.buckets[b].lock().rt_mutex_waiters.len(), 1);

        assert_eq!(guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert!(table.buckets[b].lock().rt_mutex_waiters.is_empty());
    }

    /// A PI-mutex waiter must learn whether it was still queued.
    ///
    /// `lock_pi` relies on that answer: a waiter that `unlock_pi` dequeued has been made the
    /// owner of the mutex, and must report success even if its wait failed.
    #[fuchsia::test]
    fn test_remove_rt_mutex_waiter_reports_whether_it_was_queued() {
        let table = FutexTable::default();
        let key = test_key(0x1000);
        let b = FutexTable::bucket_index(&key);

        let node1 = WaiterNode::new_stack_event(key, u32::MAX, 1, b as u16);
        let _eg1 = node1.event().begin_wait();
        let guard1 =
            StackWaiterGuard::insert_rt_mutex(&table, &mut table.buckets[b].lock(), &node1);
        assert_eq!(guard1.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));

        let node2 = WaiterNode::new_stack_event(key, u32::MAX, 1, b as u16);
        let _eg2 = node2.event().begin_wait();
        let guard2 =
            StackWaiterGuard::insert_rt_mutex(&table, &mut table.buckets[b].lock(), &node2);

        // The handoff performed by `unlock_pi` takes the waiter out of the queue.
        {
            let mut bucket = table.buckets[b].lock();
            let popped = bucket.pop_rt_mutex_waiter(&key).unwrap();
            // SAFETY: `popped` was unlinked from `bucket` under the current lock acquisition.
            unsafe {
                bucket.wake_and_mark_woken(popped);
            }
        }

        assert_eq!(guard2.resolve(error!(ETIMEDOUT)), Ok(()));
    }

    #[fuchsia::test]
    fn test_requeue_fairness() {
        let table = FutexTable::default();
        let key1 = test_key(0x1000);
        let key2 = test_key(0x2000);
        let b1 = FutexTable::bucket_index(&key1);

        let n1 = WaiterNode::new_stack_event(key1, 1, 0, b1 as u16);
        let n2 = WaiterNode::new_stack_event(key1, 2, 0, b1 as u16);
        let n3 = WaiterNode::new_stack_event(key1, 3, 0, b1 as u16);
        let _e1 = n1.event().begin_wait();
        let _e2 = n2.event().begin_wait();
        let _e3 = n3.event().begin_wait();

        let (g1, g2, g3) = {
            let mut bucket = table.buckets[b1].lock();
            (
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n1),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n2),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n3),
            )
        };

        let requeued = table.requeue_by_key(key1, key2, 0, 2, || Ok(())).unwrap();
        assert_eq!(requeued, 2);

        // First two waiters should now be on key2 in FIFO order; third remains on key1.
        assert_eq!(wake_key(&table, &key2, 1, 1), 1);
        assert_eq!(g1.resolve(Ok(())), Ok(()));
        assert_eq!(wake_key(&table, &key2, 1, 2), 1);
        assert_eq!(g2.resolve(Ok(())), Ok(()));
        assert_eq!(wake_key(&table, &key1, 1, 3), 1);
        assert_eq!(g3.resolve(Ok(())), Ok(()));
    }

    #[fuchsia::test]
    fn test_cross_bucket_requeue_with_wake_and_cancel() {
        let table = FutexTable::default();
        let key1 = test_key(0x1000);
        let b1 = FutexTable::bucket_index(&key1);
        let key2 = (2..4096)
            .map(|i| test_key(i * 0x1000))
            .find(|k| FutexTable::bucket_index(k) != b1)
            .unwrap();
        let b2 = FutexTable::bucket_index(&key2);

        let n1 = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
        let n2 = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
        let n3 = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
        let _e1 = n1.event().begin_wait();
        let _e2 = n2.event().begin_wait();
        let _e3 = n3.event().begin_wait();

        let (g1, g2, g3) = {
            let mut bucket = table.buckets[b1].lock();
            (
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n1),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n2),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n3),
            )
        };

        // Wake 1 waiter on key1 (`n1`) and requeue the remaining 2 (`n2`, `n3`) to `key2` (`b2`).
        let requeued = table.requeue_by_key(key1, key2, 1, 2, || Ok(())).unwrap();
        assert_eq!(requeued, 3);
        assert_eq!(g1.resolve(Ok(())), Ok(()));
        assert!(table.buckets[b1].lock().waiters.is_empty());
        assert_eq!(table.buckets[b2].lock().waiters.len(), 2);

        // Cancel `n2` after it moved to `b2`, then wake `n3` on `key2`.
        assert_eq!(g2.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        assert_eq!(table.buckets[b2].lock().waiters.len(), 1);
        assert_eq!(wake_key(&table, &key2, 1, FUTEX_BITSET_MATCH_ANY), 1);
        assert_eq!(g3.resolve(Ok(())), Ok(()));
        assert!(table.buckets[b2].lock().waiters.is_empty());
    }

    #[fuchsia::test]
    fn test_stale_external_waiter_cleanup() {
        let table = FutexTable::default();
        let vmo = MemoryObject::from(zx::Vmo::create(4096).unwrap());
        let key = FutexKeyValue::shared(&vmo, 0);
        let key2 = FutexKeyValue::shared(&vmo, 4);
        let b = FutexTable::bucket_index(&key);

        {
            let token = Arc::new(());
            let (sender, _receiver) = oneshot::channel::<()>();
            let waiter = UnattachedWaiter::new_external(
                key,
                u32::MAX,
                b as u16,
                Arc::downgrade(&token),
                sender,
            );
            table.buckets[b].lock().waiters.push_back(waiter);
        } // token is dropped here, so it becomes stale

        assert_eq!(table.buckets[b].lock().waiters.len(), 1);
        assert_eq!(wake_key(&table, &key, 1, FUTEX_BITSET_MATCH_ANY), 0);
        assert!(
            table.buckets[b].lock().waiters.is_empty(),
            "Stale external waiter should be removed by wake"
        );

        {
            let token = Arc::new(());
            let (sender, _receiver) = oneshot::channel::<()>();
            let waiter = UnattachedWaiter::new_external(
                key,
                u32::MAX,
                b as u16,
                Arc::downgrade(&token),
                sender,
            );
            table.buckets[b].lock().waiters.push_back(waiter);
        } // token is dropped here, so it becomes stale

        assert_eq!(table.buckets[b].lock().waiters.len(), 1);
        assert_eq!(table.requeue_by_key(key, key2, 0, 1, || Ok(())).unwrap(), 0);
        assert!(
            table.buckets[b].lock().waiters.is_empty(),
            "Stale external waiter should be removed by requeue"
        );

        {
            let token = Arc::new(());
            let (sender, _receiver) = oneshot::channel::<()>();
            let waiter = UnattachedWaiter::new_external(
                key,
                u32::MAX,
                b as u16,
                Arc::downgrade(&token),
                sender,
            );
            table.buckets[b].lock().waiters.push_back(waiter);
        } // token is dropped here, so it becomes stale

        assert_eq!(table.buckets[b].lock().waiters.len(), 1);
        let (_token2, _receiver2) = table.external_wait(vmo, 0, 0, u32::MAX).unwrap();
        assert_eq!(
            table.buckets[b].lock().waiters.len(),
            1,
            "Stale external waiter should be removed by external_wait"
        );
    }

    #[fuchsia::test]
    fn test_drop_futex_table_with_pending_external_waiters() {
        let vmo1 = MemoryObject::from(zx::Vmo::create(4096).unwrap());
        let vmo2 = MemoryObject::from(zx::Vmo::create(4096).unwrap());
        let (_token1, mut receiver1);
        let mut receiver2;
        {
            let table = FutexTable::default();
            (_token1, receiver1) = table.external_wait(vmo1, 0, 0, u32::MAX).unwrap();
            let (token2, r2) = table.external_wait(vmo2, 4, 0, u32::MAX).unwrap();
            drop(token2);
            receiver2 = r2;
        }
        // Dropping `table` must drop the boxed `External` waiter nodes and their `oneshot::Sender`s.
        assert_eq!(receiver1.try_recv(), Err(oneshot::Canceled));
        assert_eq!(receiver2.try_recv(), Err(oneshot::Canceled));
    }

    /// Waiters with different keys that hash to the same bucket must not interfere.
    #[fuchsia::test]
    fn test_same_bucket_different_keys_do_not_interfere() {
        let table = FutexTable::default();
        let key1 = test_key(0x1000);
        let b = FutexTable::bucket_index(&key1);
        let key2 = (2..4096)
            .map(|i| test_key(i * 0x1000))
            .find(|k| FutexTable::bucket_index(k) == b)
            .unwrap();

        let n1 = WaiterNode::new_stack_event(key1, u32::MAX, 0, b as u16);
        let n2 = WaiterNode::new_stack_event(key2, u32::MAX, 0, b as u16);
        let _e1 = n1.event().begin_wait();
        let _e2 = n2.event().begin_wait();

        let (g1, g2) = {
            let mut bucket = table.buckets[b].lock();
            (
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n1),
                StackWaiterGuard::insert_waiter(&table, &mut bucket, &n2),
            )
        };

        // Waking key1 must only wake n1 in the same bucket.
        assert_eq!(wake_key(&table, &key1, 1, FUTEX_BITSET_MATCH_ANY), 1);
        assert_eq!(g1.resolve(Ok(())), Ok(()));
        assert_eq!(table.buckets[b].lock().waiters.len(), 1);

        assert_eq!(wake_key(&table, &key2, 1, FUTEX_BITSET_MATCH_ANY), 1);
        assert_eq!(g2.resolve(Ok(())), Ok(()));
        assert!(table.buckets[b].lock().waiters.is_empty());
    }

    /// Private futexes at the same virtual address in two different processes (`MemoryManager`s)
    /// must not collide.
    #[::fuchsia::test]
    async fn test_private_futex_same_address_different_processes_do_not_interfere() {
        use crate::testing::spawn_kernel_and_run;

        spawn_kernel_and_run(async move |current_task| {
            let child_task = current_task.clone_task_for_test(0, None);
            let addr =
                FutexAddress::try_from(UserAddress::from((RESTRICTED_ASPACE_BASE + 0x1000) as u64))
                    .unwrap();

            let mm1 = current_task.mm().unwrap();
            let mm2 = child_task.mm().unwrap();
            let key1 = PrivateFutexKey::get(&mm1, addr).unwrap();
            let key2 = PrivateFutexKey::get(&mm2, addr).unwrap();
            assert_ne!(key1, key2);

            let table = FutexTable::default();
            let b1 = FutexTable::bucket_index(&key1);
            let b2 = FutexTable::bucket_index(&key2);

            let n1 = WaiterNode::new_stack_event(key1, u32::MAX, 0, b1 as u16);
            let n2 = WaiterNode::new_stack_event(key2, u32::MAX, 0, b2 as u16);
            let _e1 = n1.event().begin_wait();
            let _e2 = n2.event().begin_wait();

            let g1 = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b1].lock(), &n1);
            let g2 = StackWaiterGuard::insert_waiter(&table, &mut table.buckets[b2].lock(), &n2);

            assert_eq!(wake_key(&table, &key1, 1, FUTEX_BITSET_MATCH_ANY), 1);
            assert_eq!(g1.resolve(Ok(())), Ok(()));
            assert_eq!(g2.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
        })
        .await;
    }

    /// `unlock_pi` must hand the mutex to the next live waiter after an earlier waiter cancelled,
    /// preserve `FUTEX_WAITERS` while further waiters remain, and restore the waiter via
    /// `unpop_rt_mutex_waiter` if the futex word CAS fails.
    #[::fuchsia::test]
    async fn test_unlock_pi_hands_over_to_next_waiter_after_cancelled_one() {
        use crate::mm::memory::MemoryObject;
        use crate::mm::{DesiredAddress, MappingName, MappingOptions, PAGE_SIZE, ProtectionFlags};
        use crate::testing::spawn_kernel_and_run;

        spawn_kernel_and_run(async move |current_task| {
            let mm = current_task.mm().unwrap();
            let addr = mm
                .map_memory(
                    DesiredAddress::Any,
                    Arc::new(MemoryObject::from(zx::Vmo::create(*PAGE_SIZE).unwrap())),
                    0,
                    *PAGE_SIZE as usize,
                    ProtectionFlags::READ | ProtectionFlags::WRITE,
                    MappingOptions::empty(),
                    MappingName::None,
                )
                .expect("map failed");
            let futex_addr = FutexAddress::try_from(addr).unwrap();

            // The current task owns the mutex and has waiters queued behind it.
            let owner_tid = current_task.get_tid() as u32;
            assert!(matches!(
                mm.atomic_compare_exchange_u32_acq_rel(futex_addr, 0, owner_tid | FUTEX_WAITERS),
                CompareExchangeResult::Success
            ));

            const CANCELLED_TID: u32 = 0x111;
            const NEXT_OWNER_TID: u32 = 0x222;
            const THIRD_WAITER_TID: u32 = 0x333;
            let futex_table = FutexTable::default();
            let key = PrivateFutexKey::get(&mm, futex_addr).unwrap();
            let b = FutexTable::bucket_index(&key);

            let next_node = WaiterNode::new_stack_event(key, u32::MAX, NEXT_OWNER_TID, b as u16);
            let _next_eg = next_node.event().begin_wait();
            let third_node = WaiterNode::new_stack_event(key, u32::MAX, THIRD_WAITER_TID, b as u16);
            let _third_eg = third_node.event().begin_wait();

            {
                let cancelled_node =
                    WaiterNode::new_stack_event(key, u32::MAX, CANCELLED_TID, b as u16);
                let _cancelled_guard = StackWaiterGuard::insert_rt_mutex(
                    &futex_table,
                    &mut futex_table.buckets[b].lock(),
                    &cancelled_node,
                );
                // `_cancelled_guard` drops here, unlinking `cancelled_node` automatically.
            }

            let next_guard = StackWaiterGuard::insert_rt_mutex(
                &futex_table,
                &mut futex_table.buckets[b].lock(),
                &next_node,
            );
            let third_guard = StackWaiterGuard::insert_rt_mutex(
                &futex_table,
                &mut futex_table.buckets[b].lock(),
                &third_node,
            );

            // Make the mapping read-only so `unlock_pi` can read `expected_value`, pop `next_node`,
            // fail the handoff CAS with `EACCES`, and restore `next_node` via
            // `unpop_rt_mutex_waiter` (clearing its `FUTEX_WAITERS` bit).
            mm.protect(current_task, addr, *PAGE_SIZE as usize, ProtectionFlags::READ)
                .expect("protect READ failed");
            assert_eq!(
                futex_table.unlock_pi::<PrivateFutexKey>(current_task, addr),
                error!(EACCES)
            );
            assert_eq!(futex_table.buckets[b].lock().rt_mutex_waiters.len(), 2);

            mm.protect(
                current_task,
                addr,
                *PAGE_SIZE as usize,
                ProtectionFlags::READ | ProtectionFlags::WRITE,
            )
            .expect("protect READ|WRITE failed");
            futex_table.unlock_pi::<PrivateFutexKey>(current_task, addr).expect("unlock_pi failed");

            assert_eq!(
                mm.atomic_load_u32_relaxed(futex_addr).unwrap(),
                NEXT_OWNER_TID | FUTEX_WAITERS,
                "the mutex should be handed to NEXT_OWNER_TID with FUTEX_WAITERS still set"
            );
            assert_eq!(next_guard.resolve(error!(ETIMEDOUT)), Ok(()));
            assert_eq!(third_guard.resolve(error!(ETIMEDOUT)), error!(ETIMEDOUT));
            assert!(futex_table.buckets[b].lock().rt_mutex_waiters.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_futex_deadlock_with_pager() {
        use crate::mm::memory::MemoryObject;
        use crate::mm::{DesiredAddress, MappingName, MappingOptions, PAGE_SIZE, ProtectionFlags};
        use crate::testing::spawn_kernel_and_run;
        use std::sync::atomic::{AtomicBool, Ordering};
        use zx::sys::zx_page_request_command_t::ZX_PAGER_VMO_READ;

        spawn_kernel_and_run(async move |current_task| {
            let mm = current_task.mm().unwrap();

            let port = Arc::new(zx::Port::create());
            let port_clone = port.clone();
            let pager =
                Arc::new(zx::Pager::create(zx::PagerOptions::empty()).expect("create failed"));
            let pager_clone = pager.clone();

            let vmo = Arc::new(
                pager
                    .create_vmo(zx::VmoOptions::RESIZABLE, &port, 1, *PAGE_SIZE)
                    .expect("create_vmo failed"),
            );
            let vmo_clone = vmo.clone();

            let mapped_addr = mm
                .map_memory(
                    DesiredAddress::Any,
                    Arc::new(MemoryObject::from(
                        (*vmo).duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                    )),
                    0,
                    *PAGE_SIZE as usize,
                    ProtectionFlags::READ | ProtectionFlags::WRITE,
                    MappingOptions::empty(),
                    MappingName::None,
                )
                .expect("map failed");

            let futex_table = Arc::new(FutexTable::default());
            let futex_table_clone = futex_table.clone();
            let task_clone = current_task.task.clone();

            let page_requested = Arc::new(AtomicBool::new(false));
            let wake_completed = Arc::new(AtomicBool::new(false));

            let page_req_clone = page_requested.clone();
            let wake_completed_clone = wake_completed.clone();

            let pager_thread = std::thread::spawn(move || {
                let packet = port_clone.wait(zx::MonotonicInstant::INFINITE).expect("wait failed");
                if let zx::PacketContents::Pager(contents) = packet.contents() {
                    if contents.command() == ZX_PAGER_VMO_READ {
                        let range = contents.range();
                        page_req_clone.store(true, Ordering::SeqCst);

                        // Spawn waker thread while main thread is page-faulting before acquiring
                        // the lock.
                        let waker = std::thread::spawn(move || {
                            let _ = futex_table_clone.wake::<PrivateFutexKey>(
                                &task_clone,
                                mapped_addr,
                                1,
                                u32::MAX,
                            );
                            wake_completed_clone.store(true, Ordering::SeqCst);
                        });

                        // Verify waker completes immediately without being blocked by the page
                        // fault.
                        waker.join().unwrap();
                        assert!(
                            wake_completed.load(Ordering::SeqCst),
                            "Waker thread must NOT be blocked while page fault is in progress!"
                        );

                        // Supply pages to unblock the main thread's page fault
                        let source_vmo =
                            zx::Vmo::create(range.end - range.start).expect("create failed");
                        pager_clone
                            .supply_pages(&vmo_clone, range, &source_vmo, 0)
                            .expect("supply_pages failed");
                    }
                }
            });

            // This will pre-fault mapped_addr before locking the FutexTable
            let _ = futex_table.wait::<PrivateFutexKey>(
                current_task,
                mapped_addr,
                0,
                u32::MAX,
                zx::MonotonicInstant::from_nanos(1),
            );

            pager_thread.join().unwrap();
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_wait_expired_deadline_returns_etimedout_without_queuing() {
        use crate::mm::memory::MemoryObject;
        use crate::mm::{DesiredAddress, MappingName, MappingOptions, PAGE_SIZE, ProtectionFlags};
        use crate::testing::spawn_kernel_and_run;

        spawn_kernel_and_run(async move |current_task| {
            let mm = current_task.mm().unwrap();
            let addr = mm
                .map_memory(
                    DesiredAddress::Any,
                    Arc::new(MemoryObject::from(zx::Vmo::create(*PAGE_SIZE).unwrap())),
                    0,
                    *PAGE_SIZE as usize,
                    ProtectionFlags::READ | ProtectionFlags::WRITE,
                    MappingOptions::empty(),
                    MappingName::None,
                )
                .expect("map failed");

            let futex_table = FutexTable::default();
            let key = PrivateFutexKey::get(&mm, FutexAddress::try_from(addr).unwrap()).unwrap();
            let b = FutexTable::bucket_index(&key);

            assert_eq!(
                futex_table.wait::<PrivateFutexKey>(
                    current_task,
                    addr,
                    0,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::MonotonicInstant::ZERO,
                ),
                error!(ETIMEDOUT)
            );
            assert_eq!(
                futex_table.wait::<PrivateFutexKey>(
                    current_task,
                    addr,
                    0,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::MonotonicInstant::get(),
                ),
                error!(ETIMEDOUT)
            );
            assert_eq!(
                futex_table.wait_boot::<PrivateFutexKey>(
                    current_task,
                    addr,
                    0,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::BootInstant::ZERO,
                    zx::BootDuration::ZERO,
                ),
                error!(ETIMEDOUT)
            );
            assert_eq!(
                futex_table.wait_boot::<PrivateFutexKey>(
                    current_task,
                    addr,
                    0,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::BootInstant::get(),
                    zx::BootDuration::ZERO,
                ),
                error!(ETIMEDOUT)
            );
            assert!(futex_table.buckets[b].lock().waiters.is_empty());

            // A mismatched value must still report EAGAIN ahead of an expired deadline.
            assert_eq!(
                futex_table.wait::<PrivateFutexKey>(
                    current_task,
                    addr,
                    1,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::MonotonicInstant::ZERO,
                ),
                error!(EAGAIN)
            );
            assert_eq!(
                futex_table.wait_boot::<PrivateFutexKey>(
                    current_task,
                    addr,
                    1,
                    FUTEX_BITSET_MATCH_ANY,
                    zx::BootInstant::ZERO,
                    zx::BootDuration::ZERO,
                ),
                error!(EAGAIN)
            );
            assert!(futex_table.buckets[b].lock().waiters.is_empty());
        })
        .await;
    }
}
