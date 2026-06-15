---
name: cpp-to-rust-reviewer
description: >
  Guidelines and instructions for reviewing C++ to Rust ports in Zircon,
  ensuring strict API parity, algorithmic equivalence, safety, and test
  coverage matching.
---

# C++ to Rust Porting Review Guidelines

This skill defines the guidelines and procedures for a code review agent to
evaluate Rust ports of Zircon C++ code. The goal of this review is to ensure
that the Rust implementation is a high-quality, functionally equivalent, safe,
and comprehensively tested replacement for the C++ version.

The review must identify any gaps between the C++ and Rust implementations and
provide clear, actionable instructions for the implementation agent to close
them.

## Review Criteria

The review must evaluate the ported Rust code against the original C++ code
across the following dimensions:

### 1. API and Feature Parity
* **Public API**: Does the Rust version expose all public functions, methods,
  constructors, and configuration options present in the C++ version?
* **Configurability**: Are C++ template parameters, strategy patterns, and
  compile-time options (e.g., allocator flavors, synchronization primitives,
  policy flags) accurately represented in the Rust API?
* **ABI Compatibility**: If the types are shared across the FFI boundary, do the
  memory layout, alignment, and size match exactly? Are size and alignment
  verified using static assertions (`zr::static_assert!`)?

### 2. Algorithmic and Structural Equivalence
* **Data Structures**: Does the Rust version use the same underlying data
  structures as the C++ version? (e.g., matching intrusive singly/doubly linked
  lists, trees, arrays, free lists).
* **Algorithms**: Are the internal algorithms for key operations (e.g.,
  allocation, search, deletion, traversal) structurally equivalent to the C++
  version?
* **Performance Characteristics**: Does the Rust implementation preserve the
  time and space complexity of the C++ version (e.g., O(1) operations remain
  O(1))?

### 3. Memory Safety and Correctness
* **Safe vs. Unsafe**: Does the Rust code minimize `unsafe` blocks where safe
  Rust is sufficient?
* **Unsafe Correctness**: Are all `unsafe` blocks accompanied by clear `//
  SAFETY:` comments explaining why the invariants are maintained?
* **Pointer/Reference Invariants**: Are raw pointers handled correctly? Is there
  any risk of undefined behavior, data races, use-after-free, or double-free?
* **NonNull Validation**: Whenever the C++ code uses raw pointers, does the Rust
  port use `NonNull<T>` (if they must be non-null) or `Option<NonNull<T>>` (if
  they can be null) instead of raw `*const T` or `*mut T`? Does the Rust code
  enforce non-nullness at the API boundary where applicable?
* **Strict Provenance**: When casting integers (like `usize` addresses) to
  pointers, does the code use `core::ptr::with_exposed_provenance` (or
  `with_exposed_provenance_mut`) instead of `as *const T` / `as *mut T`?
* **Memory Allocation**: Are all memory allocations explicit and fallible (e.g.,
  using `kalloc::Box`)? Are there any hidden paths that could trigger a panic on
  Out-of-Memory (OOM)?

### 4. Synchronization and Concurrency
* **Locking Strategy**: Does the locking and synchronization design match the
  C++ version?
* **Deadlock Prevention**: Does the locking model prevent deadlocks in the same
  way as the C++ version? If the C++ version uses lock validation frameworks
  (e.g., lockdep), does the Rust version integrate with standard validation
  frameworks (e.g., using `ksync`)?
* **Lock Customization**: If the C++ version allows configuring the
  synchronization primitive (e.g., via a template parameter like `LockType`),
  does the Rust version support equivalent customization?

### 5. Test and Fuzz Parity
* **Coverage Equivalence**: Do the Rust unit tests cover every test case,
  scenario, and edge case present in the C++ unit tests?
* **Edge Cases**: Are limit cases, error paths, concurrency scenarios, and
  negative test cases fully tested in Rust?
* **Differential Testing**: If possible, are there tests verifying that both C++
  and Rust versions produce identical outcomes for complex inputs?
* **Fuzz Testing**: If the C++ codebase has fuzz tests (e.g.
  `raw-bitmap-fuzzer.cc`), ensure that equivalent Rust fuzzers are implemented
  using `rustc_fuzzer` and the `arbitrary` crate to fuzz the same operations.

### 6. Documentation and Comment Parity
* **Comment Matching**: Are high-level architectural descriptions, design
  constraints, and complex block comments ported from the C++ version?
* **Accuracy and Adaption**: Have C++ specific code examples, templates, and API
  references in comments been updated to reflect Rust equivalents (e.g., traits,
  const generics, standard wrappers)?
* **Safety Comments**: Are all unsafe blocks clearly documented with `//
  SAFETY:` comments explaining structural safety invariants?
* **Rustdoc Coverage**: Are all public traits, structs, enums, methods, and
  functions documented with `///` doc comments? Do `unsafe` functions have a `#
  Safety` section in their Rustdoc?

### 7. Rust Ergonomics and Idioms
* **Derive Macros**: Are standard traits (`Default`, `Clone`, `Debug`,
  `PartialEq`, `Eq`, `Hash`) and library traits derived using macros rather than
  manually implemented, unless custom logic is required?
* **Smart Pointer Ergonomics (`Deref` / `DerefMut`)**: If the C++ type behaves
  like a collection, wrapper, or smart pointer, does the Rust implementation
  implement `Deref` and `DerefMut` to expose inner methods (like `len()`,
  `is_empty()`, and indexing) rather than duplicating them manually?
* **Builder / Constructor Design**: Since Rust lacks constructor overloading,
  are multiple constructors represented idiomatically? (e.g., using default
  values via `Default`, builder pattern, or distinct descriptive constructors
  like `try_new` and `const_new`).
* **Error Handling and Optionals**: Does the API utilize Rust-idiomatic type
  wrappers (`Option<T>` instead of null pointers, and `Result<T, E>` instead of
  integer error codes)? Is the `?` operator used for clean error propagation?
  Does it use `zx_status::Status` from the `zx-status` crate instead of
  duplicating `zx_status_t` and `ZX_ERR_*` constants locally?
* **Pattern Matching**: Are nested C-style conditional chains or `switch`
  statements replaced with Rust pattern matching (`match`, `if let`,
  `let-else`)?
* **Namespace Cleanliness**: Are type imports cleaned up (using `use` statements
  at the top of the file) rather than using fully qualified paths (e.g.
  `core::sync::atomic::Ordering`) in the function body?

### 8. Code Organization Parity
* **File Structure Matching**: Does the Rust code organization (file and module
  structure) roughly match the C++ code organization?
* **Module Definition & Re-exports**: Are logically related components split
  into separate files matching their C++ counterparts, and are they correctly
  declared as modules and optionally re-exported to maintain API compatibility?
* **Test Module Declaration**: Are test files explicitly declared as modules
  (e.g., `mod tests;` under `#[cfg(test)]`) in the crate root to ensure they are
  compiled and run?

## Common Pitfalls to Check For

When reviewing ports, pay special attention to these common issues:

1.  **Unnecessary Runtime Overhead**: C++ often uses template parameters or
    conditional inheritance to completely compile out features (like statistics
    tracking or debug counters). Ensure the Rust port uses `const` generics
    (e.g., `const TRACK: bool = false`) and `const { assert!(...) }` blocks to
    compile out tracking overhead rather than hardcoding it as runtime fields.
2.  **Constructor Statistic Pollution**: Constructors that pre-allocate memory
    (e.g., calling internal allocation and free routines during construction)
    can accidentally trigger statistics counters. Ensure that pre-allocated
    structures do not pollute user-facing metrics like `max_obj_count`.
3.  **Hidden Constants**: Ensure that important compile-time values (like
    capacity, object sizes, or counts per slab) are exposed as `pub const` on
    the Rust types if they were public in C++ (e.g., `AllocsPerSlab`).
4.  **Incorrect Lock Safety Comments**: When a generic locking parameter is
    supported (e.g., allowing `NullLock` or no locking), `// SAFETY:` comments
    in `unsafe` blocks must not unconditionally assert "the lock is held". They
    should specify that "access is safe because either the lock is held, or the
    allocator has been configured for single-threaded access via NullLock."
5.  **Hardcoded Constants**: Ensure that values represented as named constants
    in the C++ version (e.g., `DEFAULT_SLAB_ALLOCATOR_SLAB_SIZE`) are not
    hardcoded in the Rust version. They should be extracted to equivalent public
    Rust constants.
6.  **Manual Derivable Trait Implementations**: Audit the code for manual
    implementations of traits that have standard derive or attribute macros
    (e.g., `SinglyLinkedListContainable`, `DoublyLinkedListContainable`,
    `Recyclable`). Ensure they are refactored to use standard derive macros
    unless custom logic is strictly necessary.
7.  **Overuse of Raw Pointers**: Porting C++ `T*` directly to Rust `*mut T` or
    `*const T` without considering nullability. If the pointer is never null, it
    should be `NonNull<T>`. If it can be null, it should be `Option<NonNull<T>>`
    to leverage Rust's type safety and null-pointer optimization.
8.  **Duplicated Zircon Status Constants**: Duplicating `zx_status_t` or any
    `ZX_ERR_*` constants locally. Check if the Rust port defines these locally,
    and instruct the implementation agent to depend on `//sdk/rust/zx-status`
    and use `zx_status::Status` instead.
9.  **Monolithic Rust File**: Porting a multi-file C++ library into a single,
    massive Rust `lib.rs` file. This makes maintenance difficult. Ensure that
    logically distinct C++ headers/implementations are mapped to separate Rust
    module files and appropriately re-exports.
10.  **Ignoring Fuzz Testing Parity**: Failing to check if the C++ codebase has
     fuzz tests and omitting equivalent Rust fuzzers. Always check for `.cc`
     files in the C++ test directories that contain `LLVMFuzzerTestOneInput` or
     use `FuzzedDataProvider`, and ensure the Rust port implements them.
11.  **Pointer Provenance Violations**: Casting integers (like raw mapped
     addresses) directly to pointers via `as *const T` or `as *mut T`. This
     violates Rust's strict provenance guidelines. Insist on using
     `core::ptr::with_exposed_provenance` or `with_exposed_provenance_mut`.

---

## Step-by-Step Review Process

The review agent must follow this structured process:

### Step 1: Locate and Read Source Files
1.  Identify the original C++ files (headers `.h` and implementation `.cc` or
    `.cpp`) and C++ test files.
2.  Identify the ported Rust files and Rust test files.
3.  Read the C++ and Rust code thoroughly.

### Step 2: Analyze the C++ Implementation
Create an inventory of:
* All public types, traits, methods, and constructors.
* All template parameters, options, and configuration flags.
* The core data structures and internal state fields.
* The synchronization model.
* The lifecycle rules (creation, destruction, reference counting).

### Step 3: Analyze the Rust Implementation
Map the Rust implementation against the C++ inventory:
* Verify that each C++ type/method has an equivalent in Rust.
* Check if all template options (e.g., flavors, options) are supported.
* Verify that compile-time static assertions are present for size and alignment
  of FFI-compatible structs.
* Analyze all `unsafe` blocks for correctness and proper safety documentation.
* Verify that block comments and usage documentation are ported from C++ and
  adapted accurately for Rust.

### Step 4: Side-by-Side Test Comparison
* List all test cases in the C++ test files.
* List all test cases in the Rust test files.
* Create a mapping to verify that every C++ test case has a corresponding Rust
  test case.
* Identify any missing test cases or scenarios in the Rust tests.

### Step 5: Document Gaps
Write down a detailed list of all gaps categorized into:
1.  **Functional / API Gaps**: Missing features, APIs, or configurations.
2.  **Algorithmic / Structural Gaps**: Mismatches in internal data structures,
    logic, or O(1) guarantees.
3.  **Safety & Correctness Gaps**: Unsafe code issues, missing safety comments,
    potential panic paths, or raw pointer misuse.
4.  **Test Gaps**: Missing unit tests, lack of edge-case testing, or unported
    test scenarios.
5.  **Documentation Gaps**: Missing block comments, outdated C++ specific
    terminology/code examples in comments, or lack of documentation parity.

### Step 6: Generate Actionable Implementation Instructions
For each identified gap, provide:
* A description of the gap.
* References to the relevant C++ and Rust files/lines.
* Specific, step-by-step instructions for the implementation agent on how to
  modify the Rust code or tests to close the gap.

---

## Review Output Format

The review agent's final response must be a structured Markdown document
containing:

1.  **Executive Summary**: A brief assessment of the current Rust port quality
    (e.g., completeness, accuracy).
2.  **Side-by-Side Comparison Tables**:
    * **API Parity**: C++ Type/Method vs. Rust Type/Method (and parity status).
    * **Test Parity**: C++ Test Case vs. Rust Test Case (and parity status).
    * **Fuzz Test Parity**: C++ Fuzzer vs. Rust Fuzzer (and parity status).
3.  **Detailed Gap Analysis**: A categorized list of gaps with thorough
    explanations.
4.  **Actionable Instructions**: The precise instructions for the implementation
    agent to fix the code and close all gaps.
