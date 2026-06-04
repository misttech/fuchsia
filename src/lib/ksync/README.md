# ksync

`ksync` is a Rust crate providing a token-based lock API, designed for
low-level, `no_std` environments like the Zircon kernel.

## Purpose

`KMutex` separates lock states from data. Instead of encapsulating protected
data inside the mutex struct itself (like `std::sync::Mutex<T>`), the data
resides in separate fields of the same struct, wrapped in `KCell`. Access to
these cells is granted only by presenting a zero-sized `LockToken` (held by the
Guard returned when locking the mutex).

This design is particularly useful when:

1.  **Multiple fields need to be protected by the same lock** but we want to
    keep them as distinct fields of the parent struct for clarity and layout
    control.

2.  **We need to support disjoint mutable borrows** of different guarded fields
    simultaneously, which is normally difficult with a single standard Mutex
    Guard.

## Ergonomic Attribute Macro: `#[guarded]`

The primary way to use this crate is via the `#[guarded]` attribute macro.

Apply `#[guarded]` to your struct and use the helper attributes:

*   `#[mutex]` to mark the `KMutex` field.
*   `#[guarded_by(mutex_field_name)]` to mark fields protected by that mutex.

### Single Mutex

When you mark a single field with `#[mutex]`, you can declare it simply as
`KMutex` (without explicit generic parameters). The macro will automatically
generate a marker lock class behind the scenes, named
`{StructName}{MutexNameCamel}Class` (e.g., `ImageCacheMuClass` for a field
named `mu` in `ImageCache`).

```rust
use ksync::{guarded, KMutex, lock};

#[derive(Default)]
#[guarded]
struct ImageCache {
    #[mutex]
    mu: KMutex,

    #[guarded_by(mu)]
    hits: u32,

    #[guarded_by(mu)]
    misses: u32,

    pub path: String,
}

fn update_cache(cache: &ImageCache, is_hit: bool) {
    // Acquire lock.
    lock!(let mut guard = cache.lock_mu());

    // Use individual accessors:
    if is_hit {
        *guard.as_mut().hits_mut() += 1;
    } else {
        *guard.as_mut().misses_mut() += 1;
    }

    // Or use split accessors for simultaneous disjoint mutable borrows:
    let fields = guard.as_mut().fields_mut();
    if is_hit {
        *fields.hits += 1;
    } else {
        *fields.misses += 1;
    }
}

fn main() {
    let cache = ImageCache {
        hits: 42.into(),
        path: "/path/to/cache".to_string(),
        ..Default::default()
    };

    update_cache(&cache, true);
}
```

### Multiple Mutexes

If your struct has multiple mutexes, the macro will automatically generate a
distinct, unique lock class for each mutex field by default (utilizing the
`{StructName}{MutexNameCamel}Class` naming convention). You don't need any
special syntax:

```rust
use ksync::{guarded, KMutex, lock};

#[derive(Default)]
#[guarded]
struct DualCache {
    #[mutex]
    mu1: KMutex,
    #[mutex]
    mu2: KMutex,

    #[guarded_by(mu1)]
    data1: u32,

    #[guarded_by(mu2)]
    data2: i32,
}

fn process_dual_cache(cache: &DualCache) {
    // Lock both mutexes.
    lock!(let mut guard1 = cache.lock_mu1());
    lock!(let mut guard2 = cache.lock_mu2());

    // Use individual accessors for each lock independently:
    *guard1.as_mut().data1_mut() = 100;
    *guard2.as_mut().data2_mut() = -50;

    // Split accessors work for each guard independently as well:
    let fields1 = guard1.as_mut().fields_mut();
    *fields1.data1 += 10;
}

fn main() {
    let cache = DualCache::default();
    process_dual_cache(&cache);
}
```

## Locked Inherent Methods

When porting C++ code that utilizes Clang's Thread Safety Analysis (e.g.,
methods annotated with `TA_REQ(mu)`) to Rust, this crate provides a safe analog
by declaring these lock-required methods directly on the generated Guard
structure instead of the parent struct.

This design pattern:

1.  **Guarantees compile-time safety**: The method cannot be physically called
    unless the caller has successfully acquired the lock and holds the Guard
    object.
2.  **Scopes lock lifetime**: The lock is scoped naturally by the Guard's
    lifetime (RAII).

### Explicit Parent Struct Access

The generated Guard structure holds a private reference `parent` pointing to
the physical parent structure instance. Since the Guard is generated inside the
same module, you can explicitly access all un-guarded fields and call
lock-free parent methods through `self.parent`:

*   **Read un-guarded fields**: `self.parent.remote_address`
*   **Call lock-free methods**: `self.parent.is_local()`

For guarded fields, the Guard's inherent target accessors (like
`self.bytes_sent()`) or split accessors (`self.fields_mut()`) provide safe
access.

### Example

```rust
#[derive(Default)]
#[guarded]
struct Connection {
    #[mutex]
    mu: KMutex,

    #[guarded_by(mu)]
    bytes_sent: u64,

    // Un-guarded state
    pub remote_address: String,
}

impl Connection {
    // Lock-free method on the parent struct
    pub fn is_local(&self) -> bool {
        self.remote_address.starts_with("127.0.0.1")
    }
}

// Locked methods declared on the generated Guard!
impl<'a> ConnectionMuGuard<'a> {
    pub fn send_packet(mut self: core::pin::Pin<&mut Self>, size: u64) {
        // 1. Access and mutate guarded fields:
        let fields = self.as_mut().fields_mut();
        *fields.bytes_sent += size;

        // 2. Explicitly read un-guarded fields via self.parent:
        if self.parent.remote_address.is_empty() {
            return;
        }

        // 3. Explicitly call lock-free methods via self.parent:
        if self.parent.is_local() {
            // ...
        }
    }
}

fn main() {
    use ksync::lock;

    let mut conn = Connection {
        remote_address: "127.0.0.1:8080".to_string(),
        ..Default::default()
    };

    // Scoped lock and execution:
    lock!(let mut guard = conn.lock_mu());
    guard.as_mut().send_packet(1024);
}
```
