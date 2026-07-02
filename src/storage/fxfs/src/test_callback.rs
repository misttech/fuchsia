// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_sync::Mutex;
use std::sync::Arc;

pub struct TestCallback(Mutex<Option<Arc<dyn Fn() + Send + Sync>>>);
pub struct TestCallbackGuard(&'static TestCallback);

impl Drop for TestCallbackGuard {
    fn drop(&mut self) {
        *self.0.0.lock() = None;
    }
}

impl TestCallback {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Returns a guard that invalidates this callback and releases the resources when dropped.
    pub fn set<F>(&'static self, callback: F) -> TestCallbackGuard
    where
        F: Fn() + Send + Sync + 'static,
    {
        let arc: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        {
            let mut inner = self.0.lock();
            assert!(inner.is_none(), "Resetting TestCallback without dropping old guard.");
            *inner = Some(arc.clone());
        }
        TestCallbackGuard(&self)
    }

    pub fn call(&self) {
        let cb = self.0.lock().as_ref().map(|cb| cb.clone());
        // Call the callback outside the lock. This isn't really a race though, since just calling
        // the callback doesn't ensure that any action will actually be done inside it, and this
        // delay to calling the callback is impossible to differentiate from that delay.
        if let Some(cb) = cb {
            cb();
        }
    }
}
