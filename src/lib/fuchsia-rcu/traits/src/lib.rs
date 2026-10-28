// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Foundational trait definitions for RCU-safe reclamation.
//!
//! This crate defines the [`RcuDroppable`] marker trait and the `#[derive(RcuDroppable)]`
//! proc-macro re-export.
//!
//! It is separate from `fuchsia-rcu` so that foundational crates can implement `RcuDroppable` for
//! their types without depending on the RCU runtime.

#![no_std]

pub use fuchsia_rcu_derive::RcuDroppable;

/// Marker trait for types that are safe to be dropped on the RCU worker / drop execution path.
///
/// # Safety
/// Types implementing this trait MUST NOT have `Drop` side-effects that block or be time or
/// thread-sensitive.
pub unsafe trait RcuDroppable: Send + 'static {}

macro_rules! impl_rcu_droppable {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: Built in numeric types, atomics, strings, and standard types without
            // custom blocking Drop logic do not block or produce thread-sensitive drop side effects.
            unsafe impl RcuDroppable for $t {}
        )*
    };
}

impl_rcu_droppable!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    str,
);

// SAFETY: `lock_api::Mutex<R, T>` has no custom Drop logic; dropping it only drops the raw
// lock `R` and the inner value `T`, both of which are `RcuDroppable`.
unsafe impl<R: RcuDroppable + lock_api::RawMutex, T: ?Sized + RcuDroppable> RcuDroppable
    for lock_api::Mutex<R, T>
{
}

// SAFETY: `lock_api::RwLock<R, T>` has no custom Drop logic; dropping it only drops the raw
// lock `R` and the inner value `T`, both of which are `RcuDroppable`.
unsafe impl<R: RcuDroppable + lock_api::RawRwLock, T: ?Sized + RcuDroppable> RcuDroppable
    for lock_api::RwLock<R, T>
{
}

#[cfg(test)]
mod tests {
    use crate::RcuDroppable;

    fn assert_rcu_droppable<T: RcuDroppable>() {}

    #[derive(RcuDroppable)]
    struct UnitStruct;

    #[test]
    fn test_rcu_droppable_trait() {
        assert_rcu_droppable::<UnitStruct>();
    }
}
