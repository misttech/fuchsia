// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::memory::MemoryObject;
use crate::task::ThreadLockupDetector;
use crate::task::dynamic_thread_spawner::{DynamicThreadSpawner, SpawnRequestBuilder};
use starnix_logging::log_debug;
use starnix_sync::{CondVar, LockDepMutex, PrefetchQueueStateLock, PrefetchWorkerReceiversLock};
use static_assertions::const_assert;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};

/// Default maximum number of bytes of requests waiting in the queue at a time (100 MiB).
const DEFAULT_PREFETCH_QUEUE_MAX_BYTES: u64 = 100 * 1024 * 1024;
const WORKER_THREAD_COUNT: usize = 2;

#[derive(Debug)]
pub struct PrefetchMessage {
    pub memory: Arc<MemoryObject>,
    pub offset: u64,
    pub length: u64,
}

struct QueueState {
    queue: VecDeque<PrefetchMessage>,
    queued_bytes: u64,
    shutdown: bool,
}

struct PrefetchQueue {
    state: LockDepMutex<QueueState, PrefetchQueueStateLock>,
    condvar: CondVar,
    max_bytes: u64,
}

impl PrefetchQueue {
    fn new(max_bytes: u64) -> Self {
        Self {
            state: LockDepMutex::new(QueueState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                shutdown: false,
            }),
            condvar: CondVar::new(),
            max_bytes,
        }
    }

    fn enqueue(&self, msg: PrefetchMessage) -> bool {
        if msg.length > self.max_bytes {
            return false;
        }
        {
            let mut state = self.state.lock();
            if state.shutdown {
                return false;
            }
            // Evict the oldest requests until there is enough quota for the new request. Don't
            // worry about overflow here as the max message length above should always be small
            // enough to avoid it. See const_assert in `PrefetchThreadPool::new()`.
            while state.queued_bytes + msg.length > self.max_bytes {
                if let Some(oldest) = state.queue.pop_front() {
                    state.queued_bytes -= oldest.length;
                } else {
                    break;
                }
            }
            state.queued_bytes += msg.length;
            state.queue.push_back(msg);
        }
        self.condvar.notify_one();
        true
    }
}

fn worker_loop(queue: &Arc<PrefetchQueue>) {
    loop {
        let msg = {
            let mut state = queue.state.lock();
            while state.queue.is_empty() && !state.shutdown {
                let _waiting_guard = ThreadLockupDetector::pause_tracking();
                queue.condvar.wait(&mut state);
            }
            if state.shutdown {
                break;
            }
            let msg = state.queue.pop_front().unwrap();
            state.queued_bytes -= msg.length;
            msg
        };

        if let Err(status) = msg.memory.op_range(zx::VmoOp::PREFETCH, msg.offset, msg.length) {
            log_debug!(status:%; "prefetch in background thread failed");
        }
    }
}

/// A thread pool for handling prefetch requests in the background. Has a maximum queue size where
/// messages that are too large are just dropped and when the queue has too many pending bytes to
/// service it drops the oldest request first since this is meant to support madvise where the
/// requests are optional, and often time sensitive.
pub struct PrefetchThreadPool {
    queue: Arc<PrefetchQueue>,
    worker_receivers: LockDepMutex<Vec<Receiver<()>>, PrefetchWorkerReceiversLock>,
}

impl PrefetchThreadPool {
    pub fn new(spawner: &DynamicThreadSpawner) -> Self {
        // Makes it impossible to overflow when checking for space so we can skip all the
        // verifications.
        const_assert!(DEFAULT_PREFETCH_QUEUE_MAX_BYTES < u64::MAX / 2);
        let queue = Arc::new(PrefetchQueue::new(DEFAULT_PREFETCH_QUEUE_MAX_BYTES));
        let mut worker_receivers = Vec::with_capacity(WORKER_THREAD_COUNT);
        for _ in 0..WORKER_THREAD_COUNT {
            let queue_clone = queue.clone();
            let (sender, receiver) = sync_channel(1);
            let req = SpawnRequestBuilder::new()
                .with_debug_name("kthread-prefetch")
                .with_sync_closure(move |_current_task| {
                    worker_loop(&queue_clone);
                    let _ = sender.send(());
                })
                .build();
            spawner.spawn_from_request(req);
            worker_receivers.push(receiver);
        }
        Self { queue, worker_receivers: LockDepMutex::new(worker_receivers) }
    }

    pub fn enqueue(&self, msg: PrefetchMessage) -> bool {
        self.queue.enqueue(msg)
    }
}

impl Drop for PrefetchThreadPool {
    fn drop(&mut self) {
        {
            let mut state = self.queue.state.lock();
            state.shutdown = true;
            state.queued_bytes = 0;
            state.queue.clear();
        }
        self.queue.condvar.notify_all();
        let receivers = std::mem::take(&mut *self.worker_receivers.lock());
        for receiver in receivers {
            let _ = receiver.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use assert_matches::assert_matches;

    #[::fuchsia::test]
    async fn test_prefetch_thread_pool_execution() {
        spawn_kernel_and_run(async |current_task| {
            let pool = PrefetchThreadPool::new(current_task.kernel().kthreads.spawner());
            let page_size = zx::system_get_page_size() as u64;

            let pager = zx::Pager::create(zx::PagerOptions::empty()).unwrap();
            let port = zx::Port::create();
            let vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, 1, page_size).unwrap();
            let child_vmo = vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0).unwrap();
            let memory = Arc::new(MemoryObject::from(child_vmo));

            let enqueued = pool.enqueue(PrefetchMessage { memory, offset: 0, length: page_size });
            assert!(enqueued);

            // Wait for pager request triggered by the worker's PREFETCH.
            let packet = port.wait(zx::MonotonicInstant::INFINITE).unwrap();
            assert_eq!(packet.key(), 1);

            let aux_vmo = zx::Vmo::create(page_size).unwrap();
            pager.supply_pages(&vmo, 0..page_size, &aux_vmo, 0).unwrap();
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_prefetch_queue_eviction() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size).unwrap();
        let memory = Arc::new(MemoryObject::from(vmo));
        let queue = PrefetchQueue::new(100);

        // A single request exceeding the quota must be dropped.
        assert!(!queue.enqueue(PrefetchMessage { memory: memory.clone(), offset: 0, length: 101 }));
        {
            let state = queue.state.lock();
            assert_eq!(state.queued_bytes, 0);
            assert!(state.queue.is_empty());
        }

        // Enqueueing requests within quota succeeds without eviction.
        assert!(queue.enqueue(PrefetchMessage { memory: memory.clone(), offset: 10, length: 40 }));
        assert!(queue.enqueue(PrefetchMessage { memory: memory.clone(), offset: 20, length: 40 }));
        {
            let state = queue.state.lock();
            assert_eq!(state.queued_bytes, 80);
            assert_eq!(state.queue.len(), 2);
            assert_matches!(&state.queue[0], PrefetchMessage { offset: 10, .. });
            assert_matches!(&state.queue[1], PrefetchMessage { offset: 20, .. });
        }

        // Adding a 40-byte request exceeds 100 bytes, evicting the oldest (offset 10).
        assert!(queue.enqueue(PrefetchMessage { memory: memory.clone(), offset: 30, length: 40 }));
        {
            let state = queue.state.lock();
            assert_eq!(state.queued_bytes, 80);
            assert_eq!(state.queue.len(), 2);
            assert_matches!(&state.queue[0], PrefetchMessage { offset: 20, .. });
            assert_matches!(&state.queue[1], PrefetchMessage { offset: 30, .. });
        }

        // Adding a 90-byte request evicts multiple older requests until it fits.
        assert!(queue.enqueue(PrefetchMessage { memory: memory.clone(), offset: 50, length: 90 }));
        {
            let state = queue.state.lock();
            assert_eq!(state.queued_bytes, 90);
            assert_eq!(state.queue.len(), 1);
            assert_matches!(&state.queue[0], PrefetchMessage { offset: 50, .. });
        }
    }

    #[::fuchsia::test]
    async fn test_drop_pool_with_outstanding_messages() {
        spawn_kernel_and_run(async |current_task| {
            let pool = PrefetchThreadPool::new(current_task.kernel().kthreads.spawner());
            let page_size = zx::system_get_page_size() as u64;
            let vmo_size = page_size * (WORKER_THREAD_COUNT as u64 + 1);

            // Create a pager-backed vmo, so that we can control the page-in threads.
            let pager = zx::Pager::create(zx::PagerOptions::empty()).unwrap();
            let port = zx::Port::create();
            let vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, 1, vmo_size).unwrap();
            let child_vmo = vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0).unwrap();
            let memory = Arc::new(MemoryObject::from(child_vmo));

            // Queue one message for each worker, and one that will never be serviced.
            for offset in
                (0..(page_size * (WORKER_THREAD_COUNT as u64 + 1))).step_by(page_size as usize)
            {
                pool.enqueue(PrefetchMessage { memory: memory.clone(), offset, length: page_size });
            }

            // Wait for the requests to be pulled off the queue. Now all workers are blocked.
            for _ in 0..WORKER_THREAD_COUNT {
                port.wait(zx::MonotonicInstant::INFINITE).unwrap();
            }

            // Dropping the pool in the background will prevent any more messages from being
            // handled by the workers as shutdown begins.
            let queue = pool.queue.clone();
            let join_handle = std::thread::spawn(move || {
                drop(pool);
            });

            // Wait for the other thread to actually commence shutdown.
            {
                let mut state = queue.state.lock();
                while !state.shutdown {
                    queue.condvar.wait(&mut state);
                }
                assert!(state.queue.is_empty());
                assert_eq!(state.queued_bytes, 0);
            }

            // Respond to the two outstanding requests. This allows the drop the complete.
            let aux_vmo = zx::Vmo::create(page_size).unwrap();
            for offset in (0..(page_size * WORKER_THREAD_COUNT as u64)).step_by(page_size as usize)
            {
                pager.supply_pages(&vmo, offset..(offset + page_size), &aux_vmo, 0).unwrap();
            }

            // Now the drop will complete.
            join_handle.join().unwrap();

            // The last request was never received.
            assert!(port.wait(zx::MonotonicInstant::INFINITE_PAST).is_err());
        })
        .await;
    }
}
