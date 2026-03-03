// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::ptr::NonNull;

use libasync_sys::*;

use crate::{AsyncDispatcherRef, OnDispatcher};

thread_local! {
    pub(crate) static OVERRIDE_DISPATCHER: RefCell<Option<NonNull<async_dispatcher_t>>> = const { RefCell::new(None) };
}

/// A placeholder for the currently active dispatcher. Use [`OnDispatcher::on_dispatcher`] to
/// access it when needed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CurrentDispatcher;

impl OnDispatcher for CurrentDispatcher {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        let dispatcher = OVERRIDE_DISPATCHER
            .with(|global| *global.borrow())
            .or_else(|| {
                // SAFETY: NonNull::new will null-check that we have a current dispatcher.
                NonNull::new(async_get_default_dispatcher().cast_mut())
            })
            .map(|dispatcher| unsafe { AsyncDispatcherRef::from_raw(dispatcher) });
        f(dispatcher)
    }
}

/// Overrides the current dispatcher used by [`dispatcher::CurrentDispatcher::on_dispatcher`] while
/// the callback is being called.
///
/// This is intended for internal use only, and only affects rust users of the default dispatcher
/// api.
#[doc(hidden)]
pub fn override_current_dispatcher<R>(
    dispatcher: AsyncDispatcherRef<'_>,
    f: impl FnOnce() -> R,
) -> R {
    OVERRIDE_DISPATCHER.with(|global| {
        let previous = global.replace(Some(dispatcher.0));
        let res = f();
        global.replace(previous);
        res
    })
}
