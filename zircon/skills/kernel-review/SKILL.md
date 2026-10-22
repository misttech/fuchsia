---
name: kernel-review
description: Perform evaluation of a CL prior to a human reviewing.
---

# Presubmit Review

## Goal

Your goal is to use the following set of steps to evaluate the current git
commit and determine if it is ready for humans to spend time reviewing.

* DO NOT perform edits to the git commit (you may temporarily change the HEAD
  and working tree during investigations, you must put everything back).
* DO NOT upload to or manipulate gerrit in any way unless instructed.

## Process

Iterate through each of the numbered evaluation steps. A step may have a
condition on whether it should be applied or not. If applied, the evaluation
step might produce a report and/or a result. These should be collected and then
handled as per the final Report step at the end. Spawn subagents as applicable
to both parallelize these steps and prevent overflowing the context window.

### Subagent Model Selection & Orchestration

To minimize `pro` model calls without impacting review quality, you are
explicitly instructed to set the `Model` argument in `invoke_subagent` according
to the task type (this serves as explicit authorization to override the default
`"inherit"` model setting):

* **Use `Model: "flash"`** for mechanical, procedural, and tool-heavy steps:
    * **Step 0**: Pre-flight Commit Triage
    * **Step 1**: Commit Message evaluation
    * **Step 2**: Build and Test execution (escalate to `pro` only to diagnose
      complex build/test failures)
    * **Step 5 (Phase A)**: Disassembly extraction (`llvm-objdump` at `HEAD` and
      `HEAD~1` baseline)
    * **Step 9**: Code Formatting checks
* **Use `Model: "inherit"` (or `"pro"`)** for deep semantic, safety, and
  assembly reasoning:
    * **Step 3**: C++ -> Rust conversion (`babeldiff`)
    * **Step 4**: Fuchsia Review (`review`)
    * **Step 5 (Phase B)**: Performance and assembly/stack analysis
    * **Step 6**: Concurrency analysis
    * **Step 7**: Provenance analysis
    * **Step 8**: Unsafe Rust review
    * **Final Report**: Overall synthesis and readiness verdict

**Context & Artifact Efficiency Rules:**

1.  **Pass Step 0 context to downstream subagents**: Include the commit message,
    changed file paths, and relevant line ranges from Step 0 in every subagent's
    prompt so `pro` subagents do not spend turns running `git status` or `git
    log` to discover the commit contents.
2.  **Write artifacts directly from subagents**: Instruct each subagent that
    produces a report to write its report artifact directly to `<Artifact
    Directory Path>/` (with `UserFacing: true`) and return a concise summary
    plus the artifact path, so the main `pro` agent does not spend extra turns
    re-writing each subagent's report.
3.  **Avoid working-tree / build-directory races**: Any step that mutates the
    build directory or working tree (Step 2 builds, Step 5 `HEAD~1`
    checkout/build, or Step 9 `fx format-code`) must not run concurrently with
    steps reading the working tree during a `HEAD~1` checkout.

### 0. Pre-flight Commit Triage (`Model: "flash"`)

Spawn a `flash` subagent (or run a single combined inspection if trivial) to
inspect `HEAD` (`git log -1`, `git show --stat`, and the diff) and return a
structured summary containing:

1.  The full commit message, subject `[tag]`, `Bug:` footer(s), and `Test:`
    footer(s).
2.  The list of modified files and changed line ranges.
3.  Required build/test architectures for Step 2 (`x86` always, plus any
    arch-specific directories modified, plus any user-requested architectures)
    and whether `//zircon/kernel` is modified.
4.  Applicability flags for conditional steps:
    * **Step 3 (babeldiff)**: Does the commit implement existing C++ logic in
      Rust? (If yes, list the corresponding C++ and Rust file paths.)
    * **Step 6 (Concurrency)**: Does the diff or affected code involve locks,
      atomics, `UnsafeCell`, parallelism, or data structures shared concurrently
      across CPUs?
    * **Step 7 (Provenance)**: Does the diff manipulate raw pointers or
      notionally implement/run an allocator?
    * **Step 8 (Unsafe)**: Does the diff add, modify, or rely on `unsafe`
      blocks, functions, traits, `#[pin_data]` / `!Unpin` pinning guarantees, or
      safe APIs in modules that uphold safety invariants?

### 1. Commit Message (`Model: "flash"`)

Delegate to a `flash` subagent to evaluate the commit message for suitability
against the following criteria:

1.  Has appropriate [tag] on the subject line.
2.  Has appropriate subject description.
3.  Contains a meaningful description, rationale and motivation of the contents
    of the commit.
4.  The commit does not contain changes that seem out of place or surprising
    based on the commit message.
5.  If the commit message has a Bug: footer check the bug and make sure that it
    exists and is a meaningful bug to use for this commit. It is fine for a Bug:
    footer to be absent.
6.  If the commit message has a Test: footer make sure the tests it references
    relevant and up to date for the current commit contents. It is fine for a
    Test: footer to be absent.

Commit messages tend to be fairly subjective. Your report should only consider
absence of a [tag] and a commit message that looks like a placeholder as being
serious issues, any other issues should be minor recommendations.

### 2. Build and Test (`Model: "flash"` for execution; `pro` for complex failure diagnosis)

If the user requested to skip building and testing (e.g., "skip build", "no
build/test"), skip this step; otherwise perform it by default.

Delegate the build and test execution to a `flash` subagent:

First determine what architectures to build and test on. Use the following
criteria:

1.  x86 should always be built and tested.
2.  If the commit changes code inside arch specific directories or files then
    also include that architecture. Note that changing common code or files that
    are used by a particular architecture is insufficient to trigger this
    requirement.
3.  If the user specifically requested any additional architectures, include
    them.

Before performing code compilation first examine the commit and determine what
tests are related to the changes and whether the relevant tests / test packages
are included in the build.

Add any missing tests to the build configuration and then perform the build.

Run all the tests you previously determined as related to the change. If there
are ANY changes to the //zircon/kernel directory you must run the full
core-tests test suite.

Once building and testing in this step are complete (and if Step 5 is not
immediately reusing the live Bazel server), run `fx bazel shutdown` to release
open file descriptors held by the Bazel server.

**Failure handling:**

* Any build failures count as a reject for this change being ready to send for
  review.
* If the build and tests succeed (or fail due to an obvious syntax/import typo),
  the `flash` subagent prepares the Step 2 report artifact directly.
* If there are non-trivial build or test failures, the `flash` subagent must
  capture the exact compiler errors or failing test logs and return them so the
  `pro` agent can diagnose the root cause and prepare recommendations on how to
  fix them.

### 3. C++ -> Rust conversion (babeldiff) (`Model: "inherit"`)

If Step 0 determined that this change looks like a Rust implementation of
existing C++ code then perform this step, otherwise skip.

Spawn a `Model: "inherit"` subagent (passing the paired C++ and Rust file paths
from Step 0) to use the babeldiff skill to perform an analysis of the conversion
accuracy and evaluate the following C++-to-Rust porting checks, preparing a
report on its findings:

* **Manual Comment Checks Beyond `babeldiff`**: Because `babeldiff` primarily
  matches top-level function headers, manually verify:
    1.  **Struct Field & Enum Variant Comments**: All doc comments on C++ struct
        fields and enum variants are copied over to the Rust definitions. When
        two C++ types are merged into one Rust type or a struct is moved between
        headers, preserve the doc comments from all source declarations.
    2.  **Non-Trivial Inline "Why" Comments**: Important algorithmic or
        invariant comments inside C++ function bodies are preserved in the Rust
        port.
    3.  **Stale Cross-References & Positional Warnings**: Check for comments
        referencing deleted C++ headers (e.g., `"Must match foo.h"`) or relative
        positions in modified/pruned headers (e.g., `"See WARNING above"` when
        the referenced block was deleted or moved).
    4.  **Adapt Identifiers in Ported/Moved Comments**: Update C++ PascalCase
        method names (e.g., `Wait`, `Post`, `Block` -> `wait`, `post`, `block`),
        field/parameter names (e.g., `|foo_level|` -> `|foo_pages|`), and
        removed getter/setter references inside ported or moved comments to
        match the current Rust/C++ code.
    5.  **No References or Comparisons to Deleted C++**: Comments in ported Rust
        must state invariants in present terms rather than describing or
        comparing against C++ code deleted in the same CL (e.g., do not cite a
        `DEBUG_ASSERT` from a deleted `.cc` file).
    6.  **Facade Header Comments, Variant Accuracy, & `TODO:` Markers**:
        * New Rust facade methods for unported C++ functions must copy the C++
          `.h` declaration doc comments verbatim rather than inventing new
          summaries.
        * Verify that comments copied between `*_locked` and unlocked variants
          accurately reflect whether the lock was just released, and do not
          reference obsolete lock types (e.g., calling a mutex a spinlock).
        * Use a `TODO:` prefix on comments noting that a temporary FFI export,
          shim, or trait impl can be removed once remaining C++ types/callers
          are ported.
* **FFI Shim & Facade Ownership (Callee, Not Caller)**: Verify that `extern "C"`
  FFI shims (`cpp_*` / `rust_*`), `*_ffi.{cc,h,rs}` declarations, and opaque
  Rust facade types (`OpaqueFacade`) live in the **callee** module/subsystem
  that owns the underlying C++ API or type—never in a caller's local
  `*_ffi.{cc,rs}` or an unrelated module. Before a CL introduces a new opaque
  facade, FFI wrapper, or bindgen-backed struct, search the tree to ensure a
  facade or Rust type does not already exist in another module.
* **`*_ffi.rs` Boundary & Encapsulation**:
    1.  `*_ffi.rs` files must strictly contain `extern "C"` declarations and
        thin C++ <-> Rust proxy entry points (`rust_*` / `cpp_*`). New Rust
        domain types, helper functions, and inherent `impl` blocks belong in the
        primary module (`foo.rs`), not `foo_ffi.rs`.
    2.  Callers outside a type's defining module (or higher-level methods) must
        not call raw `bindings::cpp_*` / `ffi::cpp_*` functions directly on a
        Rust wrapper type; expose and call methods on the Rust type itself.
    3.  **Prefer References in `rust_*` FFI Trampolines**: When C++ passes a
        non-null pointer to an initialized `#[repr(C)]`, `Freeze` Rust type
        (e.g., `Deadline`), take `&T` / `&mut T` directly in the `extern "C" fn`
        signature (and `const T&` / `T&` in C++) rather than `*const T` / `*mut
        T` (or a `c_void` bindgen alias) with manual null checks and casts.
    4.  **Keep C++ Convenience Overload Delegation in C++**: When C++ overloads
        (e.g., `WaitQueue::Block`) simply forward to a single canonical C++
        method (`BlockEtc`), keep the overloads delegating in C++ and only route
        the canonical method across FFI (`rust_*`), avoiding extra FFI entry
        points and `TA_NO_THREAD_SAFETY_ANALYSIS` annotations in C++ headers.
    5.  **DRY FFI Struct Marshalling**: When multiple `rust_*` trampolines
        convert between a Rust struct and a `bindgen`/FFI struct, factor the
        conversion into `From<RustType> for bindings::FfiType` (or a helper)
        rather than duplicating field-by-field marshalling.
* **Global Variables Across FFI (`#[link_name]`)**:
    1.  When Rust needs to read or write a C++ global/static variable, prefer
        giving the C++ variable `extern "C"` linkage and binding it directly in
        Rust with `unsafe extern "C" { #[link_name = "..."] static ...; }`
        rather than writing trivial `cpp_get_*` / `cpp_set_*` C++ FFI accessor
        functions.
    2.  Never define a duplicate Rust `static` alongside an existing C++ global
        variable without linking them via `#[link_name = "..."]`—there must only
        ever be a single source of truth for global state during incremental
        porting.
* **Preserve Existing C++ Tests & Verify 1:1 Test Case Parity**:
    1.  Existing C++ unit test files (`*_tests.cc` / `UNITTEST(...)`) must
        **not** be deleted in the same CL that ports a component or adds Rust
        unit tests, unless there is a fundamental conflict where both cannot
        coexist. C++ tests validate the FFI shims/facades and behavioral parity
        and should remain in-tree until the C++ FFI facades are pruned in a
        follow-up CL.
    2.  **1:1 Test Case & Parameterized Variant Parity**: Enumerate every `TEST`
        / `UNITTEST` in the C++ test file—and every enum variant or mode in
        parameterized test helpers (e.g., `Signal::kKill` and
        `Signal::kSuspend`)—and verify each is ported to the Rust test suite
        whenever the underlying Rust APIs are available.
    3.  **No Redundant C++ Layout `static_assert`s in `*_tests.cc`**: When
        `zr::static_assert!` (`size_of`, `align_of`, `offset_of!`) in Rust
        already verifies layout against `bindgen`, do not move or duplicate
        C++-only `static_assert(offsetof(...))` checks into `*_tests.cc`.
* **1:1 Struct & State Fidelity**: Verify field-by-field parity between C++
  structs/globals and their Rust counterparts:
    1.  **Type Changes & Atomics Require Justification**: Plain C++ fields or
        globals should not be silently changed to `Atomic*` (or duplicated in a
        parallel shadow wrapper type like `AtomicFoo` that can lose sync with
        `Foo`) when the C++ original used non-atomic reads/writes. Changing a
        type (such as using `Atomic*` because an unlocked read or concurrent
        access would otherwise be Undefined Behavior in Rust) is allowed **only
        if accompanied by a comment justifying why the change is necessary**
        (and provided it does not add unacceptable overhead inside a benchmark).
    2.  **No Dropped Fields or Downgraded Types**: Flag any omitted C++ struct
        fields (even if unused by the functions in the current CL) or helper
        types (e.g., `EventLimiter`) replaced with raw primitives.
    3.  **Derived Default Values**: Ensure default values derived from C++
        struct initializers or configuration objects (e.g.,
        `BootOptions{}.pmm_checker_action`) are not replaced with hardcoded
        constants in Rust.
    4.  **Explicit `#[repr(...)]` on `OpaqueStorage`-Backed Types**: Any Rust
        enum or struct whose layout or size is mirrored by C++
        `OpaqueStorage<SIZE, ALIGN>` must carry an explicit `#[repr(C)]`,
        `#[repr(u8)]`, or `#[repr(C, u8)]` attribute rather than relying on
        unspecified `repr(Rust)` layout.
    5.  **Pinning (`Pin`, `#[pin_data]`, `!Unpin`) Fidelity**:
        * Inside a `#[pin_data]` struct, never use an unannotated
          `PhantomData<PhantomPinned>` or bare `PhantomPinned` (which leaves the
          generated `Unpin` impl active); use `#[pin] _pin: PhantomPinned` (or
          another `#[pin]` field that is `!Unpin`).
        * When a `!Unpin` / `#[pin_data]` type is pin-initialized in place
          inside a `kalloc::Box`, store it as `Pin<kalloc::Box<T>>` (via
          `Pin::new_unchecked(uninit_box.assume_init())`) rather than stripping
          `Pin` into a plain `kalloc::Box<T>`.
        * When interior mutability (`OpaqueBytes`, `UnsafeCell`, atomics) allows
          relaxing a method on an address-sensitive or intrusive type from
          `Pin<&mut Self>` to a shared reference, keep the receiver as `self:
          Pin<&Self>` rather than `&self`.
    6.  **Strict C++ `private:` Visibility Parity**: C++ `private:` methods and
        constructors whose only Rust callers are `rust_*` FFI trampolines in the
        same `.rs` file must remain private (`fn`), not `pub(crate)` or `pub`.
* **C++ Inheritance & Class Naming**:
    1.  When porting a C++ derived class (e.g., `Derived : public Base`), do not
        flatten/inline the shared base class into the derived struct; convert
        the C++ inheritance style into a Rust trait and/or composed member to
        preserve the original intent.
    2.  When a C++ class delegates its implementation over FFI to Rust, keep the
        original C++ class name (e.g., `class RunQueue`) rather than renaming it
        to `RustFoo` (`RustRunQueue`), which implies two separate types.
    3.  Conversely, when a Rust shim/facade wraps an unported C++ concrete class
        (e.g., `VmSlotPageStorage`, `VmLz4Compressor`), name the Rust struct
        after the C++ class directly rather than `CppFoo` plus a `pub type Foo =
        CppFoo` alias.
* **No Manual Duplication of SDK/Shared Definitions & Proper `bindgen` Usage**:
    1.  Do not manually redeclare constants, bitflags, or structs from
        `sdk/lib/...` or `zircon/system/public/...` (e.g., `zbi-format`) inside
        kernel `*_ffi.rs` files; use existing crates in `sdk/lib/rust` or
        generate bindings in the owning library.
    2.  When `bindgen` is used, ensure it follows standard Fuchsia bindgen
        workflows rather than ad-hoc checked-in copies paired with
        `LINT.IfChange` / `LINT.ThenChange` comments against C headers.
    3.  Do not place Rust constant or type definitions in `raw_lines` inside
        `kernel_bindgen_crate` (`BUILD.gn`); define them in a `.rs` module file.
    4.  Define object sizes and offsets in `object-constants.h` symbolically
        relative to base constants (e.g., `kBaseOffset + kBaseSize`) rather than
        hardcoded magic numbers.
* **Algorithmic Complexity Parity & Idiomatic Rust Signatures**:
    1.  Verify that container and data-structure operations (such as intrusive
        list `splice`, `size`/`len`, or tree traversals) do not degrade an
        $O(1)$ C++ operation into an $O(N)$ traversal in Rust.
    2.  When a C++ pointer parameter cannot be `nullptr` (unconditionally
        dereferenced or `DEBUG_ASSERT`ed non-null), the Rust signature should
        take `&T` / `&mut T` rather than `Option<&T>`.
    3.  Flag newly added methods on a type that duplicate the exact type
        signature and semantics of an existing accessor on that type (including
        methods already exposed via `Deref<Target = Base>`).
    4.  Prefer existing in-tree utilities (`pin_init::pin_init_array_from_fn`,
        `fbl::StringBuffer`, `DurationMono::from_seconds(1).into_nanos()`,
        `UnsafeCell::get_mut()` when holding `Pin<&mut Self>`) over hand-rolled
        `unsafe` closures, custom buffer types, magic numbers (`1_000_000_000`),
        or raw pointer dereferences in `Drop`.
* **Behavioral, Debug-Switch, Branch-Hint, & Control-Flow Parity**:
    1.  **`unlikely(...)` Branch Hints**: Preserve C++ `unlikely(...)` branch
        hints using `core::hint::cold_path()` in Rust.
    2.  **`ASSERT` vs. `DEBUG_ASSERT` & Release-Build Null Checks**: Flag
        unintended upgrades of `DEBUG_ASSERT` to `ASSERT` or `debug_assert!` to
        `assert!` (especially when `assert!` calls an FFI function like
        `ints_disabled()` in a hot path), and flag replacing a C++ release-build
        silent return on null (`DEBUG_ASSERT(p); if (!p) return;`) with an
        unconditional `.unwrap()`.
    3.  **Compile-Time Debug Switches (`LK_DEBUGLEVEL`,
        `ZX_DEBUG_ASSERT_IMPLEMENTED`)**: Do not hardcode C++ `LK_DEBUGLEVEL`
        checks (e.g., `WAIT_QUEUE_VALIDATION`) to `false` in Rust; wire them to
        `cfg!(...)`. In C++ headers using `<zircon/assert.h>`, use `#if
        ZX_DEBUG_ASSERT_IMPLEMENTED` consistently.
    4.  **Control-Flow, Panic Formatting, & Helper Parity**: Keep function
        boundaries and control flow aligned with C++ (avoid inventing extra
        helper functions like `flush_range` or `#[cold] fn unexpected_*` when
        inline code or `panic!` suffices, and flag edits to existing functions
        outside the scope of the port). Use existing string-formatting helpers
        (e.g., `page_state_to_string(state)`) instead of `{:?}` (`Debug`) when
        matching C++ panic/log messages, and omit trailing `\n` in Rust `panic!`
        messages.
* **Orphaned Cross-Language FFI, C++ Header, & `bindgen` Dead-Code Audit**:
  Audit both the ported module and any collaborating modules for code made dead
  by the port:
    1.  **Dead C++ Methods & FFI Shims**: Remove C++ declarations, `.cc`
        forwarding shims, and `rust_*` / `cpp_*` FFI entry points that have no
        remaining C++ callers once callers move to Rust, and tighten the
        corresponding Rust functions' visibility (`fn` or `pub(super)`).
    2.  **Orphaned FFI-Only Parameters & Conversion Helpers**: Remove Rust
        helpers or parameters that only existed to serve removed C++ FFI callers
        (e.g., C++-supplied `now` timestamps on `*_with_ticks` wrappers, unused
        `as_raw` / `from_raw` methods, or unused parameters on manual
        `rust_*_init` functions).
    3.  **Layout-Only C++ Types & `OpaqueStorage`**: When C++ no longer
        constructs, embeds, or takes `sizeof(T)` of a ported type (only passing
        `T&` / `T*`) or no longer uses an auxiliary struct/enum across FFI
        (`Stats`, `State`, `StateLayout`, `OpaqueStorage` fields), remove the
        C++ definitions, `bindgen` allowlist entries, and `zr::static_assert!`
        checks that kept them alive solely for layout assertions.
    4.  **Empty `*_ffi.h` Headers, Unused `#include`s, & Stale `bindgen`
        Rules**: Delete emptied `*_ffi.h` headers, remove unused `#include`s and
        forward declarations from modified `.h`/`.cc` files, and prune redundant
        `function_allowlist = []` or stale `type_blocklist` entries in
        `kernel_bindgen_crate` (`BUILD.gn`).

### 4. Fuchsia Review (`Model: "inherit"`)

Spawn a `Model: "inherit"` subagent (passing the changed file list from Step 0)
to use the standard 'review' skill to perform a code review and forward the
report on its findings. In addition to the standard review checks, ensure the
subagent evaluates:

* **Bug-Fix Regression Tests**: If the commit is a bug fix or behavioral change
  (rather than a pure refactoring/porting change) and does not include a new or
  updated test, check whether a userspace core test
  (`//zircon/system/utest/core/...`) or kernel unit test (`#[unittest::test]`)
  can be written to demonstrate the fix or behavior change.
* **Cold-Path Hint Placement Analysis (`core::hint::cold_path()`)**:
  Independently of C++ parity, analyze conditional branches across added or
  modified Rust functions to identify rare, slow-path, fallback, error/abort,
  contention, or diagnostic/logging branches inside hot or inline functions
  where calling `core::hint::cold_path()` would improve branch layout and keep
  fast-path code generation compact.
* **Cross-Cutting FFI, API, and Comment Hygiene** (especially when Step 3 was
  skipped, e.g. on FFI-pruning or standalone Rust/C++ CLs):
    * FFI shims/facades live in the callee module, `*_ffi.rs` files only proxy
      C++ <-> Rust without defining domain types/methods, and callers invoke
      methods on Rust types rather than raw `bindings::cpp_*` calls.
    * No orphaned C++ methods, `rust_*`/`cpp_*` FFI shims, `as_raw`/`from_raw`
      helpers, layout-only C++ structs/enums (`OpaqueStorage`/`StateLayout` when
      C++ only uses `T&`/`T*`), empty `*_ffi.h` headers, unused
      `#include`s/forward declarations, or redundant `kernel_bindgen_crate`
      allowlists/blocklists.
    * No manual duplication of `sdk/lib/...` definitions, misuse of `bindgen`
      with `LINT.IfChange`, or Rust definitions inside `bindgen` `raw_lines`.
    * Proper `Pin` / `#[pin_data]` hygiene (`#[pin] _pin: PhantomPinned` rather
      than unpinned `PhantomData<PhantomPinned>`, `Pin<kalloc::Box<T>>` for
      heap-pinned `!Unpin` types, and `self: Pin<&Self>` on shared methods of
      pinned types).
    * No broken relative comments (e.g., `"See WARNING above"` after header
      pruning), stale identifier/parameter names in moved comments, missing
      `TODO:` prefixes on temporary shims, degraded $O(N)$ container operations,
      `Option<&T>` parameters where `None` is invalid, duplicate accessors
      (including `Deref` duplicates), or verbose repeated module paths where a
      `use` import suffices.

### 5. Performance and Code Generation (`Model: "flash"` for disassembly collection -> `Model: "inherit"` for analysis)

Determine the requested performance analysis level (`none`, `brief`, or
`thorough`):

* **None** (default if not requested): Skip this step.
* **Brief** (when the user requests a "brief" performance analysis, or requests
  a performance analysis without specifying "thorough"):
    * **Reuse the existing build configuration** (from Step 2, or the current
      build directory if Step 2 was skipped). Do **not** run `fx set`, change
      build arguments, or compile additional architectures.
    * **At most one additional incremental build (baseline without the CL):**
        1.  First, using the output from Step 2 (`HEAD`)—or building only the
            specific affected target(s) at `HEAD` if Step 2 was
            skipped—disassemble **only** the specific modified or added
            functions (`llvm-objdump --disassemble-symbols=<sym>`).
        2.  If a before/after comparison is applicable (e.g. modifying existing
            functions or converting C++ -> Rust), you may temporarily checkout
            `HEAD~1`, perform **one** incremental build of only the specific
            affected target(s), extract the disassembly for the baseline
            symbols, and immediately restore `HEAD`. (Ensure no other steps or
            subagents are concurrently reading the working tree while `HEAD~1`
            is checked out.)
    * **No experimental edits or rebuilds:** Do **not** modify code or data
      structures to test potential optimizations; recommend improvements based
      on your analysis of the source and disassembly.
* **Thorough** (only when the user explicitly requests a "thorough" performance
  analysis):
    * Analyze both x86 and arm objdumps for this change (building both
      architectures if not already built).
    * If the change is meant to be behavior-preserving (for example if it's a
      C++ -> Rust conversion) then evaluate for performance parity against the
      baseline.
    * If introducing new logic and behaviors, check if the code generation is
      obviously 'bad' and if there are minor code / data changes that could
      improve it, testing candidate changes if needed.

**Two-Phase Execution for Step 5:**

1.  **Phase A — Disassembly Collection (`Model: "flash"`)**: Use a `flash`
    subagent (run after Step 2 completes) to locate the compiled binary/object
    files, resolve mangled symbol names for the modified/added functions, run
    `llvm-objdump --disassemble-symbols=<sym>` at `HEAD` (and `HEAD~1` if
    applicable, restoring `HEAD` immediately), run `fx bazel shutdown` when all
    builds are finished, and write the extracted disassembly to scratch files in
    `<Artifact Directory Path>/scratch/`.
2.  **Phase B — Assembly & Stack Analysis (`Model: "inherit"`)**: Using the
    `pro` model (either directly in the orchestrator or via a `Model: "inherit"`
    subagent in a single pass over the scratch disassembly files and source
    diff), evaluate the disassembly. Be especially mindful of unnecessary or
    excessive stack allocations / usage in kernel code, and check whether
    rare/fallback/diagnostic branches inlined into hot paths need
    `core::hint::cold_path()` to move cold code out of the hot instruction
    stream. Prepare a report on any issues and potential improvements that could
    be made.

### 6. Concurrency (`Model: "inherit"`)

If Step 0 determined that the code change or its surrounding context involves
concurrency, parallelism, locks, atomics, `UnsafeCell`, or data structures
shared across CPUs, perform this step; otherwise skip.

Spawn a `Model: "inherit"` subagent to examine the code change with an eye for
concurrency and parallelism. Items you are specifically evaluating:

* Use of locking is appropriate and minimal.
* Lock scopes are not widened across operations (such as `thread.resume()`) that
  C++ performed outside the lock, atomic loads are not hoisted out of
  conditional branches, and redundant lock assertions (e.g.,
  `assert_lock_not_held` immediately after `assert_no_transaction_active`) are
  avoided.
* Runtime trace arguments (such as `PerCpu` or active transaction reads for
  `ktrace`) are evaluated lazily inside the trace macro rather than eagerly
  before checking whether the trace category is enabled.
* 'manual' concurrency using UnsafeCell looks correct.
* No shared reference (`&Self`) or lifetime-unified lock guard remains in scope
  across a thread-blocking call (e.g., `scheduler::block` /
  `block_etc_postamble`) or lock release after which another CPU/thread may
  destroy the object, and `PinnedDrop::drop(self: Pin<&mut Self>)` does not
  violate `# Safety` aliasing claims while background threads still hold
  `&Self`.
* Data structures that might be used concurrently across CPUs are structured to
  minimize false contention.
* Memory orders on atomic operations. If converting existing C++ memory orders
  should exactly match the C++. Any memory order that is not Sequentially
  Consistent should have a justification as to why this is not the case.

Prepare a report on your findings.

### 7. Provenance (`Model: "inherit"`)

If Step 0 determined that the change is manipulating raw pointers or notionally
running some kind of allocator and hence might need to be examined for
provenance, continue; otherwise skip.

Spawn a `Model: "inherit"` subagent to perform a careful analysis of the changed
code and existing code against the rules of Rust provenance and prepare a
thorough report, specifically checking:

* **No Read-Only Reference Round-Trips**: Never convert a `NonNull<T>` or `*mut
  T` into a shared reference `&T` and then cast back to `*const T` / `*mut T`
  (e.g., `core::ptr::from_ref(x.foo()).cast_mut()`) when the stored pointer can
  be returned directly via `.as_ptr()`.
* **Pointer Cast Hygiene**: Prefer `core::ptr::from_ref(x)` /
  `core::ptr::from_mut(x)` with `.cast::<T>()`, `.cast_const()`, `.cast_mut()`,
  and `MaybeUninit::as_mut_ptr()` over chained `as *const _ as *mut _` casts or
  `Box::as_ptr(&uninit_box).cast()`.
* **Borrow Lifetimes Across Blocking / Deallocation**: Ensure references
  materialized from raw pointers in FFI trampolines or blocking helpers end
  their lexical/lifetime scope before any call (`scheduler::block`, unlock, or
  free) during which another CPU may destroy or mutate the pointee.

### 8. Unsafe (`Model: "inherit"`)

If Step 0 determined that the change adds, modifies, or interacts with `unsafe`
code or safety invariants, perform this step; otherwise skip.

Spawn a `Model: "inherit"` subagent to use the SKILL
https://github.com/google/rust-skills/blob/main/unsafe_rust_review/SKILL.md to
perform a review of the safety impact of this change and forward any report. In
addition to the standard `unsafe_rust_review` checks, explicitly evaluate:

* **Safe API Loopholes & Subobject Invariant Bypass**: Check whether safe
  constructors, methods, or `Deref` coercions in the module allow safe callers
  to violate safety invariants without writing `unsafe`:
    1.  Safe initializers accepting raw magic/discriminant values (e.g.,
        `init(magic: u32)`) that could trigger an unchecked C++ downcast
        (`static_cast<Derived*>`).
    2.  Safe methods on a base type reachable via `Deref` or `.as_base()` (e.g.,
        `WaitQueueBase::collection_mut`) that bypass an `unsafe` invariant on a
        derived type (`OwnedWaitQueue`).
    3.  Safe token reborrow helpers returning a by-value token (`reborrow(&mut
        self) -> Token`) that can be passed to a lock-releasing (`TA_REL`)
        method without disarming the owning guard (causing a double-release on
        `Drop`).
* **Propagating Subobject & Transitive C++ Preconditions into `# Safety`**:
  Ensure `pub unsafe fn`s on base types document conditional derived-type
  (`OwnedWaitQueue`) invariants in `# Safety` and at `// SAFETY:` call sites,
  and ensure FFI facade functions document transitive C++ scheduler and
  transaction preconditions (`THREAD_RUNNING`, blocking permitted,
  `AssertNumLocksHeld(1)`, `ChainLockTransaction` not finalized on entry and
  finalized on exit).
* **FFI Callback Trait Execution Context & Non-Aliasing**: When a safe Rust
  trait is invoked from C++ with interrupts disabled or locks/transactions held,
  require `# Execution Context / Panics` docs on the trait and a caller `#
  Safety` precondition that no conflicting proof/guard (`ChainLockProof` /
  `OwnerGuard`) is live across the call.
* **Pinning (`#[pin_data]`, `Pin<kalloc::Box<T>>`) & `PinnedDrop` Soundness**:
  Verify `#[pin_data]` structs use `#[pin] _pin: PhantomPinned` (never unpinned
  `PhantomData<PhantomPinned>`), heap-allocated `!Unpin` types retain
  `Pin<kalloc::Box<T>>`, and `PinnedDrop::drop(self: Pin<&mut Self>)` does not
  violate `# Safety` aliasing claims while background threads hold `&Self`.
* **`// SAFETY:` Placement & Accuracy**: Verify that `// SAFETY:` comments only
  appear immediately above an `unsafe` block or `unsafe impl` (never above safe
  assignments) and that any code expressions cited in `// SAFETY:` match the
  actual code.

### 9. Code Formatting (`Model: "flash"`)

Delegate to a `flash` subagent:

Use the 'format-code' fx tool to check that the current commit has been
correctly formatted (ensure any working-tree changes made by `fx format-code`
are reverted after capturing the diff so the working tree remains clean). For
any `.rs`, `.cc`, and `.h` files in the commit, explicitly check all added lines
to make sure they are `<=100` characters in width—including comments (`//`,
`///`) and code/strings inside macro invocations (`dprintf!`, `kprintln!`,
`assert!`, etc.) that `fx format-code` (`rustfmt`) does not wrap.

For Rust in places where doc comments can go (above functions, structs, members,
etc) if there is a '//' comment determine if that is a mistake or should be a
doc comment. In `///` doc comments, check that a blank `///` line separates any
numbered or bulleted list from a subsequent paragraph.

Scan added comments, strings, and the commit message for common Gerrit
Spellchecker warnings (e.g., `"re-use"` instead of `"reuse"`, `"defered"`
instead of `"deferred"`), and check whether C++ methods modified to call `const`
FFI functions can now be marked `const` (`clang-tidy`).

Summarize any issues in a report.

## Report

Each of the evaluation steps, if they needed to be run, will have produced their
own report artifact (written directly by the subagent or by you). Examine all of
the step reports and provide a summarized version of any issues or actionable
resolutions along with an overall recommendation on whether this change is ready
for human review.
