// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Marker trait for types that are safe to be dropped on the RCU worker / drop execution path.
///
/// # Safety
/// Types implementing this trait MUST NOT have Drop side-effects that block or are time or thread
/// sensitive.
pub unsafe trait RcuDroppable: Send + 'static {}

macro_rules! impl_rcu_droppable {
    ($($t:ty),* $(,)?) => {
        $(
            // SAFETY: Primitive numeric types, atomics, strings, and standard types without
            // custom Drop logic do not block or produce drop side effects.
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
    String,
    str,
    bstr::BString,
    bstr::BStr,
    std::collections::hash_map::RandomState,
    std::num::NonZeroU8,
    std::num::NonZeroU16,
    std::num::NonZeroU32,
    std::num::NonZeroU64,
    std::num::NonZeroU128,
    std::num::NonZeroUsize,
    std::num::NonZeroI8,
    std::num::NonZeroI16,
    std::num::NonZeroI32,
    std::num::NonZeroI64,
    std::num::NonZeroI128,
    std::num::NonZeroIsize,
    std::sync::atomic::AtomicBool,
    std::sync::atomic::AtomicI8,
    std::sync::atomic::AtomicI16,
    std::sync::atomic::AtomicI32,
    std::sync::atomic::AtomicI64,
    std::sync::atomic::AtomicIsize,
    std::sync::atomic::AtomicU8,
    std::sync::atomic::AtomicU16,
    std::sync::atomic::AtomicU32,
    std::sync::atomic::AtomicU64,
    std::sync::atomic::AtomicUsize,
    std::ops::RangeFull,
    std::time::Duration,
    std::time::Instant,
);

// SAFETY: Static references have no drop implementation and thus no drop side effects.
unsafe impl<T: ?Sized + Sync + 'static> RcuDroppable for &'static T {}
// SAFETY: BuildHasherDefault is a zero-sized type with no drop side effects.
unsafe impl<H: 'static> RcuDroppable for std::hash::BuildHasherDefault<H> {}
// SAFETY: PhantomData has no drop side effects.
unsafe impl<T: ?Sized + Send + 'static> RcuDroppable for std::marker::PhantomData<T> {}
// SAFETY: Option<T> only drops its inner value T when present, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for Option<T> {}
// SAFETY: Result<T, E> only drops its inner T or E value, both of which are RcuDroppable.
unsafe impl<T: RcuDroppable, E: RcuDroppable> RcuDroppable for Result<T, E> {}
// SAFETY: Box<T> deallocates heap memory and drops T, which is RcuDroppable.
unsafe impl<T: ?Sized + RcuDroppable> RcuDroppable for Box<T> {}
// SAFETY: Arc<T> decrements the reference count and only drops T (which is RcuDroppable + Sync) when the last strong reference is released.
unsafe impl<T: ?Sized + RcuDroppable + Sync> RcuDroppable for std::sync::Arc<T> {}
// SAFETY: Weak<T> decrements the weak reference count without dropping the inner T value.
unsafe impl<T: ?Sized + Send + Sync + 'static> RcuDroppable for std::sync::Weak<T> {}
// SAFETY: Vec<T> deallocates buffer memory and drops elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for Vec<T> {}
// SAFETY: Arrays drop their elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable, const N: usize> RcuDroppable for [T; N] {}
// SAFETY: Slice drops its elements of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for [T] {}
// SAFETY: std::sync::mpsc::Sender closes the channel on drop without blocking.
unsafe impl<T: Send + 'static> RcuDroppable for std::sync::mpsc::Sender<T> {}
// SAFETY: std::sync::mpsc::SyncSender closes the channel on drop without blocking.
unsafe impl<T: Send + 'static> RcuDroppable for std::sync::mpsc::SyncSender<T> {}
// SAFETY: futures::channel::mpsc::Sender closes the channel on drop without blocking.
unsafe impl<T: Send + 'static> RcuDroppable for futures::channel::mpsc::Sender<T> {}
// SAFETY: futures::channel::mpsc::UnboundedSender closes the channel on drop without blocking.
unsafe impl<T: Send + 'static> RcuDroppable for futures::channel::mpsc::UnboundedSender<T> {}
// SAFETY: futures::channel::oneshot::Sender cancels the channel on drop without blocking.
unsafe impl<T: Send + 'static> RcuDroppable for futures::channel::oneshot::Sender<T> {}
// SAFETY: AtomicPtr has a trivial drop and does not own or drop the pointee.
unsafe impl<T: 'static> RcuDroppable for std::sync::atomic::AtomicPtr<T> {}
// SAFETY: RcuPtr has a trivial drop and does not own or drop the pointee.
unsafe impl<T: 'static> RcuDroppable for crate::rcu_ptr::RcuPtr<T> {}

// Range types
// SAFETY: Range only drops its start and end fields of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for std::ops::Range<T> {}
// SAFETY: RangeFrom only drops its start field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for std::ops::RangeFrom<T> {}
// SAFETY: RangeInclusive only drops its start and end fields of type T, which are RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for std::ops::RangeInclusive<T> {}
// SAFETY: RangeTo only drops its end field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for std::ops::RangeTo<T> {}
// SAFETY: RangeToInclusive only drops its end field of type T, which is RcuDroppable.
unsafe impl<T: RcuDroppable> RcuDroppable for std::ops::RangeToInclusive<T> {}

// RCU containers
// SAFETY: RcuArc drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuArc<T> {}
// SAFETY: RcuBox drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuBox<T> {}
// SAFETY: RcuOptionArc drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuOptionArc<T> {}
// SAFETY: RcuOptionBox drops its inner type with rcu_drop which is safe if T: RcuDroppable
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuOptionBox<T> {}
// SAFETY: RcuWeak holds a non owning reference and won't deallocate T on rcu_drop.
unsafe impl<T: RcuDroppable + Sync> RcuDroppable for crate::RcuWeak<T> {}

// Synchronization primitives
// SAFETY: Mutex drops its inner value T, which is RcuDroppable.
unsafe impl<T: ?Sized + RcuDroppable> RcuDroppable for fuchsia_sync::Mutex<T> {}
// SAFETY: RwLock drops its inner value T, which is RcuDroppable.
unsafe impl<T: ?Sized + RcuDroppable> RcuDroppable for fuchsia_sync::RwLock<T> {}

// Tuples
// SAFETY: 1-element tuple drops its constituent element, which is RcuDroppable.
unsafe impl<A: RcuDroppable> RcuDroppable for (A,) {}
// SAFETY: 2-element tuple drops its constituent elements, which are RcuDroppable.
unsafe impl<A: RcuDroppable, B: RcuDroppable> RcuDroppable for (A, B) {}
// SAFETY: 3-element tuple drops its constituent elements, which are RcuDroppable.
unsafe impl<A: RcuDroppable, B: RcuDroppable, C: RcuDroppable> RcuDroppable for (A, B, C) {}
// SAFETY: 4-element tuple drops its constituent elements, which are RcuDroppable.
unsafe impl<A: RcuDroppable, B: RcuDroppable, C: RcuDroppable, D: RcuDroppable> RcuDroppable
    for (A, B, C, D)
{
}
