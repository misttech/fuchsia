---
name: ctorustportingpatternsinzircon
description: >
  Patterns and guidelines for porting Zircon C++ code to Rust, ensuring memory
  layout matching, fallible allocation, and ergonomic design.
---

# C++ to Rust Porting Patterns in Zircon

This skill documents the patterns and guidelines applied when porting Zircon
kernel and library code from C++ to Rust.

## Core Principles

1.  **Direct Translation**: Translate C++ code to Rust using exactly the same
    data structures and algorithms where possible.
2.  **Memory Layout Parity**: The memory layout of Rust structs must match
    corresponding C++ objects exactly.
3.  **Test Parity**: Test coverage for Rust code must match C++ code exactly.
    Always cross-check Rust test coverage against the corresponding C++ tests
    and close any gaps by adding more tests to the Rust side.
4.  **Ergonomic Design**: The Rust code should be ergonomic and follow Rust best
    practices where they don't conflict with layout or behavior requirements.
5.  **DRY Principle**: Apply "Don't Repeat Yourself" to minimize duplication.
6.  **Fallible Allocation**: All allocations in kernel mode must be explicit and
    fallible. Panics on OOM are unacceptable.
7.  **Locking and Synchronization**: The locking strategies and concurrency
    control protocols must match the C++ code.

## Patterns

### 1. Memory Layout Matching

To ensure Rust structs can be shared with or replace C++ objects:
- Use `#[repr(C)]` on structs.
- Use compile-time assertions to verify size and alignment.
- Use the `zr::static_assert!` macro from the `zr` crate.

Example:
```rust
#[repr(C)]
pub struct Canary<const MAGIC: u32> {
    magic: u32,
}

zr::static_assert!(core::mem::size_of::<Canary<0>>() == 4);
zr::static_assert!(core::mem::align_of::<Canary<0>>() == 4);
```

Also add matching static asserts in C++ test files to double check
compatibility.

### 2. Fallible Allocation

For collections or structures that allocate memory:
- Do not use the standard Rust `alloc` crate directly in kernel mode, as it
  panics on OOM.
- Use the fallible allocation functions provided by `kalloc` to ensure OOM
  conditions are handled without panicking.
- `kalloc` delegates to kernel `malloc`/`free` in kernel mode and standard
  `alloc` in userspace/tests.
- Use `kalloc::Box` for fallible allocation of sized types and slices when
  ownership management is needed.

### 3. Zero-Dependency Core (`zr`)

Foundational primitives that do not depend on other crates should be placed in
the `zr` crate (e.g., `static_assert`).

### 4. Cross-Language Testing

To verify that Rust implementations are compatible with C++:
- Write FFI helpers in C++ tests to verify Rust objects.
- Example: A C++ function that takes a pointer to a Rust-created object and
  calls a C++ method on it to verify state.

### 5. Unsafe Code and SAFETY Comments

When using `unsafe` blocks, always add a `// SAFETY:` comment explaining why the
block is safe. This is especially important when porting C++ code where memory
safety depends on invariants not checked by the compiler.

### 6. Workaround for Generic Const Expressions

Stable Rust does not allow generic parameters in const operations (e.g., `[u8; N
+ 1]`). When porting C++ templates that use a capacity `N` and allocate `N + 1`
  elements for a null terminator:
- Let the Rust generic parameter `N` represent the **total size** of the backing
  array.
- Document that a C++ `Type<M>` corresponds to a Rust `Type<{M + 1}>`.
- The Rust implementation will have a capacity of `N - 1`.

### 7. Ergonomic Design via `Deref`

When porting collection-like or string-like C++ structures:
- Avoid porting methods like `data()`, `length()`, or custom indexing directly
  if they can be provided by `Deref` and `DerefMut` to a slice (e.g., `[u8]` or
  `[T]`).
- Implementing `Deref` automatically provides `len()`, `is_empty()`, and
  standard indexing, making the Rust code more idiomatic.

### 8. Avoid Redundant Initialization

When initializing an array with `[0; N]`, do not explicitly set elements to `0`
immediately after, as they are already zero-initialized.

### 9. Namespace Usage

Prefer adding `use` directives at the top of the file rather than writing out
fully qualified namespaces (e.g., `core::ffi::CStr`) for everything explicitly.
This makes the code more compact and readable.
