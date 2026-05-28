// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use libasync_sys::*;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Weak;

use crate::dispatcher_arc::DispatcherArc;
use crate::{AsyncDispatcherRef, OnDispatcher};

/// A weak reference to a dispatcher whose lifetime is bound by the dispatcher itself.
#[derive(Clone, Debug)]
pub struct WeakDispatcher(
    // Note: the static lifetime here is actually constrained by the lifetime of the Weak, which
    // ensures it is no longer accessible after the lifetime of the actual dispatcher reference.
    Weak<AtomicPtr<async_dispatcher_t>>,
);

impl WeakDispatcher {
    /// Creates a weak reference to this dispatcher. The [`WeakDispatcher`] returned will only
    /// resolve until the dispatcher is being shut down. This is ensured at runtime by registering
    /// a callback that resolves only when the dispatcher shuts down.
    ///
    /// Note that creating a new one from scratch is potentially an expensive operation, since it
    /// requires registering a callback with the dispatcher. If you will be creating a lot of weak
    /// dispatcher references, you should probably try to centralize the initial creation and then
    /// vend weak pointers from that.
    pub fn new(dispatcher: impl OnDispatcher) -> WeakDispatcher {
        dispatcher.on_dispatcher(|dispatcher| {
            let Some(dispatcher) = dispatcher else {
                return WeakDispatcher(Weak::new());
            };
            // SAFETY: We transmute away the local lifetime of the DispatcherRef because
            // DispatcherArc will manage its lifetime to prevent access after the dispatcher has
            // been shut down.
            let inner = DispatcherArc::new(AtomicPtr::new(dispatcher.0.as_ptr()));
            WeakDispatcher(inner.into_weak(dispatcher))
        })
    }
}

impl OnDispatcher for WeakDispatcher {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        let Some(dispatcher_ptr) = self.0.upgrade() else {
            return f(None);
        };
        let Some(dispatcher) = NonNull::new(dispatcher_ptr.load(Ordering::Acquire)) else {
            return f(None);
        };
        // SAFETY: As long as we hold the strong reference in dispatcher_ptr, the
        // DispatcherArc will not allow its drop to finish and the dispatcher should still
        // be valid.
        f(Some(unsafe { AsyncDispatcherRef::from_raw(dispatcher) }))
    }
}
