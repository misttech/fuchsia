// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;

/// A token that proves that the lock for `Class` is held.
///
/// This is a zero-sized type that cannot be constructed safely outside of this crate.
pub struct LockToken<'a, Class> {
    _marker: PhantomData<&'a Class>,
    _phantom: PhantomData<*const ()>,
}

impl<'a, Class> LockToken<'a, Class> {
    /// Create a new token.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the mutual exclusion lock for the associated `Class` is
    /// currently held by the calling thread, and will remain held for the lifetime `'a` of the
    /// returned token.
    #[inline]
    pub unsafe fn new() -> Self {
        Self { _marker: PhantomData, _phantom: PhantomData }
    }
}
