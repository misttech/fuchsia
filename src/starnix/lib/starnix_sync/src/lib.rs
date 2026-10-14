// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synchronization objects used by Starnix

mod atomic_time;
mod condvar;
mod interruptible_event;
mod lock_dep_mutex;
mod lock_ordering;
mod lock_traits;
mod locks;
mod port_event;
mod rw_seq_lock;
mod thread_affinity;

pub use atomic_time::*;
pub use condvar::*;
pub use interruptible_event::*;
pub use lock_dep_mutex::*;
pub use lock_ordering::*;
pub use lock_ordering_macro::*;
pub use lock_traits::*;
pub use locks::*;
pub use port_event::*;
pub use rw_seq_lock::*;
pub use thread_affinity::*;

// This allows lock_ordering! macro to use paths within this crate
// by referring to them by the external crate name.
extern crate self as starnix_sync;

/// The duration a blocking wait must sleep before `clean_stack()` is called to decommit unused
/// stack pages.
pub(crate) const STACK_CLEAN_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

/// Computes the initial deadline for a blocking wait so that waits longer than `clean_delay` wake
/// once after `clean_delay` to call `starnix_stack::clean_stack()` before sleeping until
/// `deadline`.
#[inline]
pub(crate) fn initial_clean_deadline(
    deadline: zx::MonotonicInstant,
    clean_delay: zx::MonotonicDuration,
) -> zx::MonotonicInstant {
    // Avoid querying the monotonic clock when the deadline is already in the past (e.g.
    // `INFINITE_PAST` or `ZERO` on non-blocking polls).
    if deadline > zx::MonotonicInstant::ZERO {
        std::cmp::min(deadline, zx::MonotonicInstant::after(clean_delay))
    } else {
        deadline
    }
}
