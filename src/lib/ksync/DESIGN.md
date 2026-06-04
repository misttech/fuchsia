# KMutex: Technical Design Document

This document covers the technical architecture, safety invariants, and macro
code generation details of the `ksync` crate.

## 1. Technical Architecture

`ksync` implements a **token-based synchronization pattern** (also known as
the "Ghost Token" pattern). Instead of encapsulating the protected data inside
the mutex struct itself (like `std::sync::Mutex<T>`), it separates the lock
state (`KMutex`) from the actual data storage (`KCell`).

```
+-------------------------------------------------------------+
|                        Parent Struct                        |
|                                                             |
|   +----------------+           +------------------------+   |
|   |  KMutex<Class> |           |  KCell<T, Class>       |   |
|   |  (Raw Lock)    |           |  (UnsafeCell wrapper)  |   |
|   +--------+-------+           +-----------+------------+   |
+------------|-------------------------------|----------------+
             | lock()                        | get(token)
             v                               v
    +--------+------------+         +--------+-------+
    |  KMutexGuard        |         |  &T / &mut T   |
    |                     |-------->|  (Safe Access) |
    |  - LockToken        |         +----------------+
    +---------------------+
```

### The Three Core Types

1.  **`LockToken<'a, Class>`**: A zero-sized type (ZST) that serves as
    compile-time proof that the exclusive lock for `Class` is currently held by
    the current thread. It has a lifetime `'a` bound to the active
    `KMutexGuard`.
2.  **`KMutex<Class>`**: The lock state representation. It wraps a raw mutex
    (e.g., `fuchsia_sync::RawMutex`). Locking it acquires the raw lock and
    returns a `KMutexGuard` holding the ZST `LockToken`.
3.  **`KCell<T, Class>`**: A wrapper around `core::cell::UnsafeCell<T>`.  It
    provides token-gated accessors (`get(&self, token)` and `get_mut(&self,
    token)`).

### Guard Types & The Instance-Bound Soundness Model

While the `LockToken` and `KCell` share a compile-time `Class` type parameter
to prevent mixing locks of different types, this type-level association is
insufficient to guarantee memory safety.

To provide safety, the macro generates Guard structures (`MyStructMuGuard`)
that provide a safe wrapper that provides the instance-level association. The
Guard holds a reference to the specific parent struct instance (`parent: &'a
MyStruct`) and the active lock state (`inner: KMutexGuard<'a,
MyStructMuClass>`), providing the link needed to make accessing each `KCell`
safe.

## 2. Safe Exclusive Access

If a thread has unique exclusive access to the `KCell` container itself
(either by holding ownership `self` or an exclusive borrow `&mut self`), it
doesn't need a runtime lock token to access the data safely. The Rust borrow
checker already guarantees compile-time thread exclusivity:

*   **`get_inner_mut(&mut self) -> &mut T`**: Accesses the inner value mutably
    via `UnsafeCell::get_mut()`. This is safe because the `&mut self` borrow
    ensures no other borrows of the cell are active.
*   **`into_inner(self) -> T`**: Consumes the `KCell` container and returns the
    inner value `T` safely via `UnsafeCell::into_inner()`.

## 3. Macro Code Generation

The `#[guarded]` attribute proc-macro parses a struct definition, rewrites its
fields, and generates the accompanying safe Guard and Split Accessor
structures.

### 3.1 Input Struct

```rust
#[guarded]
pub struct MyStruct {
    #[mutex]
    pub mu: KMutex,

    #[guarded_by(mu)]
    pub data1: u32,

    #[guarded_by(mu)]
    data2: i32,
}
```

### 3.2 Expanded Code Output (Simplified)

```rust
// 1. Unique Lock Class marker struct generated automatically
pub struct MyStructMuClass;

// 2. Struct fields rewritten to KMutex<Class> and KCell<T, Class>
pub struct MyStruct {
    pub mu: ::ksync::KMutex<MyStructMuClass>,
    pub data1: ::ksync::KCell<u32, MyStructMuClass>,
    data2: ::ksync::KCell<i32, MyStructMuClass>,
}

// 3. Custom Guard generated with safe target accessors and lifetime bindings
#[pin_init::pin_data(PinnedDrop)]
pub struct MyStructMuGuard<'a> {
    parent: &'a MyStruct,
    #[pin]
    inner: ::ksync::KMutexGuard<'a, MyStructMuClass>,
}

#[pin_init::pinned_drop]
impl<'a> pin_init::PinnedDrop for MyStructMuGuard<'a> {
    fn drop(self: ::core::pin::Pin<&mut Self>) {
        // Releases the raw lock automatically when dropped
    }
}

impl<'a> MyStructMuGuard<'a> {
    // Individual Accessors (matching original field visibilities)
    #[inline]
    pub fn data1(&self) -> &u32 {
        // SAFETY: The token is obtained from the same parent instance (self.parent)
        // that contains the cell, satisfying the KCell safety invariant.
        unsafe { self.parent.data1.get(self.inner.token()) }
    }

    #[inline]
    pub fn data1_mut(self: ::core::pin::Pin<&mut Self>) -> &mut u32 {
        // SAFETY: Safe projection to pinned inner guard to get mutable token.
        let me = unsafe { self.get_unchecked_mut() };
        let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
        unsafe { me.parent.data1.get_mut(inner_pin.token_mut()) }
    }

    #[inline]
    fn data2(&self) -> &i32 {
        // SAFETY: The token is obtained from the same parent instance (self.parent)
        // that contains the cell, satisfying the KCell safety invariant.
        unsafe { self.parent.data2.get(self.inner.token()) }
    }

    #[inline]
    fn data2_mut(self: ::core::pin::Pin<&mut Self>) -> &mut i32 {
        // SAFETY: Safe projection to pinned inner guard to get mutable token.
        let me = unsafe { self.get_unchecked_mut() };
        let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
        unsafe { me.parent.data2.get_mut(inner_pin.token_mut()) }
    }

    #[inline]
    pub fn fields<'b>(&'b self) -> MyStructMuFields<'b> {
        MyStructMuFields {
            // SAFETY: The token is from the same parent instance as the cell.
            data1: unsafe { self.parent.data1.get(self.inner.token()) },
            data2: unsafe { self.parent.data2.get(self.inner.token()) },
            _marker: ::core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn fields_mut<'b>(self: ::core::pin::Pin<&'b mut Self>) -> MyStructMuFieldsMut<'b> {
        let me = unsafe { self.get_unchecked_mut() };
        let inner_pin = unsafe { ::core::pin::Pin::new_unchecked(&mut me.inner) };
        let token = inner_pin.token_mut();
        // SAFETY:
        // 1. We have exclusive access to the Guard.
        // 2. The fields in the struct are disjoint.
        // 3. The returned references are bound to the lifetime 'b of the guard borrow.
        unsafe {
            MyStructMuFieldsMut {
                data1: &mut *me.parent.data1.as_mut_ptr(token),
                data2: &mut *me.parent.data2.as_mut_ptr(token),
                _marker: ::core::marker::PhantomData,
            }
        }
    }
}

// 4. Helper split structs generated to hold the disjoint borrows
pub struct MyStructMuFields<'b> {
    pub data1: &'b u32,
    data2: &'b i32,
    _marker: ::core::marker::PhantomData<(&'b (), )>,
}

pub struct MyStructMuFieldsMut<'b> {
    pub data1: &'b mut u32,
    data2: &'b mut i32,
    _marker: ::core::marker::PhantomData<(&'b (), )>,
}

// 5. Lock method impl on the parent struct returning a PinInit
impl MyStruct {
    #[inline]
    pub fn lock_mu(&self) -> impl pin_init::PinInit<MyStructMuGuard<'_>, ::core::convert::Infallible> {
        pin_init::pin_init!(MyStructMuGuard {
            parent: self,
            inner <- ::ksync::KMutexGuard::new(&self.mu),
        })
    }
}
```
