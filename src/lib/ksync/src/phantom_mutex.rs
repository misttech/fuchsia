// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use pin_init::pin_data;

/// A zero-sized raw mutex marker that does not perform physical locking.
///
/// `PhantomMutex` can be used as the raw lock type in a `KMutex<Class, PhantomMutex>` field
/// to serve as a zero-sized "phantom" lock in structs where physical synchronization is managed
/// by an external shared lock (such as a ref-counted `PeerHolder` for peered dispatchers).
///
/// Using `KMutex<Class, PhantomMutex>` allows structs to integrate with `#[guarded]` and
/// `#[guarded_by(...)]` without allocating memory for a physical mutex in every instance.
/// Because `PhantomMutex` has no physical state of its own, associating a `PhantomMutex`-guarded
/// struct with a physical mutex (`lock_<mu>`) or lock token (`guard_<mu>` / `guard_<mu>_mut`) is
/// `unsafe` and requires the caller to uphold that the provided mutex or token is the specific
/// lock instance protecting that struct.
///
/// # Example
///
/// ```
/// use ksync::{KCell, KMutex, PhantomMutex, guarded};
///
/// #[guarded]
/// struct RealObject {
///     #[mutex]
///     mu: KMutex,
/// }
///
/// #[guarded]
/// struct PeeredObject {
///     #[mutex(RealObjectMuClass)]
///     mu: KMutex<PhantomMutex>,
///     #[guarded_by(mu)]
///     value: KCell<i32>,
/// }
///
/// // When locking the shared physical mutex (`real_obj.mu`), call `lock_mu` in an `unsafe`
/// // block (asserting that `real_obj.mu` is the lock protecting `phantom_obj`):
///
///     ksync::lock!(let guard = unsafe { phantom_obj.lock_mu(&real_obj.mu) });
///     let value = guard.value();
///
/// // If already holding a lock token from another guard of the same class, call `guard_mu`
/// // in an `unsafe` block to obtain an accessor object without re-locking:
///
///     let other_value = unsafe { other_phantom_obj.guard_mu(guard.token()) }.value();
/// ```
#[pin_data]
#[derive(Default, Debug, Clone, Copy)]
pub struct PhantomMutex;
