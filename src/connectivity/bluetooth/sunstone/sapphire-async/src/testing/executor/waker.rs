// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::task::{RawWaker, RawWakerVTable, Waker};

use crate::testing::executor::TaskHeader;

// The raw waker vtable managing task wakeups.
const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, wake_task, wake_task, drop_waker);

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &VTABLE)
}

// SAFETY: Wakes the task by pushing its index back into the scope's run queue.
unsafe fn wake_task(ptr: *const ()) {
    // SAFETY: ptr is a valid pointer to a TaskHeader kept alive by the Scope's heap allocations.
    let header = ptr as *const TaskHeader;
    let id = unsafe { (*header).id };
    let queue = unsafe { (*header).ready_queue.as_ref() };

    let mut queue = queue.lock();
    // Prevent duplicate enqueuing if it's already in the queue
    if !queue.contains(&id) {
        queue.push_back(id);
    }
}

unsafe fn drop_waker(_ptr: *const ()) {
    // No-op because task deallocation is managed statically by the Scope's Drop implementation.
}

pub fn make_waker(task: &TaskHeader) -> Waker {
    let raw_waker = RawWaker::new(task as *const _ as *const (), &VTABLE);
    unsafe { Waker::from_raw(raw_waker) }
}
