// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::pin::Pin;

/// A pinned, heap-allocated future that implements `Send`.
///
/// Typically used for dynamic dispatch traits (`dyn Trait`) where methods
/// return an asynchronous result that can be sent across threads.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A pinned, heap-allocated future that does not implement `Send`.
///
/// Typically used for single-threaded or thread-local dynamic dispatch traits (`dyn Trait`).
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A pinned, heap-allocated future that implements `Send + Sync`.
pub type BoxFutureSync<'a, T> = Pin<Box<dyn Future<Output = T> + Send + Sync + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_box_future() {
        let fut: BoxFuture<'_, u32> = Box::pin(async { 42 });
        assert_eq!(fut.await, 42);
    }

    #[fuchsia::test]
    async fn test_local_box_future() {
        let fut: LocalBoxFuture<'_, u32> = Box::pin(async { 42 });
        assert_eq!(fut.await, 42);
    }

    #[fuchsia::test]
    async fn test_box_future_sync() {
        let fut: BoxFutureSync<'_, u32> = Box::pin(async { 42 });
        assert_eq!(fut.await, 42);
    }
}
