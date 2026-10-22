# Link-Time Optimization (LTO) & RBE Worker Selection in Fuchsia

This document explains how Link-Time Optimization (LTO) and ThinLTO are configured
and executed across Fuchsia's build systems (GN and Bazel), and how link actions
are routed to larger Remote Build Execution (RBE) worker pools when LTO is active.

---

## 1. Overview: LTO and ThinLTO

Link-Time Optimization allows the compiler and linker to perform whole-program
optimizations across translation unit boundaries. In Fuchsia, two flavors are
supported:

- **Full LTO** (`-flto`, Rust: `-Clto=fat`): The compiler emits LLVM bitcode into
  object files (`.o`, `.rlib`). During final linking, the linker aggregates all
  transitive bitcode into a single monolithic LLVM compilation and optimization
  pipeline before emitting machine code. This achieves maximal optimization but
  requires substantial memory and CPU time at link time.
- **ThinLTO** (`-flto=thin`, Rust: `-Clto=thin`): The compiler generates bitcode
  along with compact summary indexes. The linker performs global analysis on the
  summaries, then imports cross-module functions and optimizes and codegens each
  module in parallel across multiple worker threads.

### Activation Mechanisms

In Fuchsia, LTO/ThinLTO can be enabled in two ways:

1. **Toolchain Variants**:
   - Setting `select_variant = [ "lto" ]` or `select_variant = [ "thinlto" ]`
     (or targeting specific components with variant selectors) adds `"lto"` to
     `toolchain_variant.tags`.
   - In Bazel, this tag is forwarded as
     `--//build/bazel/config/toolchain_tags:lto=True`.

2. **Compilation Optimization Modes (`optimize`)**:
   - Global or per-toolchain `optimize` settings in `//build/config/BUILD.gn`
     (and forwarded to Bazel via `--//build/bazel/config/variables:optimize=<mode>`):
     - `moderate_lto`, `size_lto`, `speed_lto`: Enable full LTO for Clang and Rust.
     - `moderate_thinlto`, `size_thinlto`, `speed_thinlto`: Enable ThinLTO for Clang and Rust.
     - `size_thinlto_clang`: Enables ThinLTO for Clang binaries only (Rust uses standard optimization).

### Compatibility Guard: `lto-if-compatible` / `thinlto-if-compatible`

Both `BUILD.gn` and `BUILD.bazel` define `lto-if-compatible` and
`thinlto-if-compatible`. When an `optimize_*_lto` or `optimize_*_thinlto` mode is
selected, LTO is automatically disabled if the toolchain is instrumented
(sanitizers like ASan, MSan, UBSan, or coverage/profiling), because combining LTO
with compiler instrumentation can lead to incompatible compiler passes or
prohibitive link times and binary sizes.

---

## 2. GN Build Configuration (`BUILD.gn`)

The configurations in `//build/config/lto/BUILD.gn` define compiler and linker
flags:

- `lto`: Enables full LTO (`-flto`, `-Clto=fat`, `-Zdylib-lto`).
- `thinlto`: Composed of `thinlto-clang` and `thinlto-rust`.
- `thinlto-clang`: Passes `-flto=thin`, limits ThinLTO threads via
  `-Wl,--thinlto-jobs=${thinlto_jobs}`, sets `-Wl,--thinlto-cache-dir=${thinlto_cache_dir}`,
  and includes `//build/config/rbe:big_rbe_machine`.
- `thinlto-rust`: Passes `-Clto=thin`, sets `--thinlto-cache-dir=${thinlto_cache_dir}`,
  and includes `//build/config/rbe:big_rbe_machine`.
- `fat-lto-objects`: Generates object files containing both native code and LLVM
  bitcode (`-ffat-lto-objects`), enabling bitcode linking when LTO is active
  while allowing standard non-LTO links to succeed without recompilation.
- `lto-workarounds`: Applies linker flags to ensure proper PIC relocation models
  and TLS descriptor generation during LTO.
- `lto-rust-linker-plugin`: Enables `-Clinker-plugin-lto` for rlibs so LLVM bitcode
  is preserved for consumption by LLD.
- `lto-whole-program-devirtualization`: Enables `-fwhole-program-vtables` for
  self-contained binaries that do not expose external C++ ABI (e.g. host tools).

### GN RBE Worker Routing

Because ThinLTO and LTO link steps are multi-threaded and require significantly
more memory (frequently exceeding standard container memory limits), link actions
can run out of memory (OOM) on default RBE workers.

In GN:
- `thinlto-clang` and `thinlto-rust` include `//build/config/rbe:big_rbe_machine`.
- `//build/config/rbe:big_rbe_machine` appends `--remote-worker-pool=large` to
  `ldflags` (when `link_rbe_enable` is true) and `rustflags` (when `rust_rbe_enable` is true).
- Fuchsia's RBE wrapper scripts (`//build/rbe/cxx_link_remote_wrapper.py` and
  `//build/rbe/rustc_remote_wrapper.py`) wrap the tool invocations. The wrapper
  script intercepts `--remote-worker-pool=large`, strips it from the compiler/linker
  command line so the underlying compiler never sees it, and forwards
  `--platform=Pool=large_worker` to `rewrapper`.

---

## 3. Bazel Build Configuration (`BUILD.bazel`)

`//build/config/lto/BUILD.bazel` mirrors the GN configurations using `build_flags()`
and `compiler_build_flags()`.

### Key Differences from GN

1. **ThinLTO Cache Directory (`--thinlto-cache-dir`)**:
   Omitted in Bazel. In GN/Ninja, ThinLTO writes intermediate object files into an
   unmanaged directory outside Ninja's dependency tracking. This is incompatible
   with Bazel's hermetic sandboxing and remote execution, where all action inputs
   and outputs must be explicitly declared and tracked.
2. **`//build/config/rbe:big_rbe_machine`**:
   Omitted from Bazel compiler and linker build flags. In Bazel, remote actions do
   not run through Fuchsia's Python wrapper scripts (`cxx_link_remote_wrapper.py`).
   Passing `--remote-worker-pool=large` in `ldflags` or `rustflags` would result
   in an unknown flag error from `lld` or `rustc`.
   Instead, Bazel natively constructs the RBE `Platform` protobuf for each action
   by merging `exec_properties` defined on the action's **execution platform** and
   the **target** (see `PlatformUtils.getPlatformProto` in Bazel).

### Routing C++ Link Actions (`CppLink`) to Large RBE Workers in Bazel

In `@rules_cc`:
- C++ rules (`cc_binary`, `cc_test`, `cc_library` shared libraries, as well as
  SDK and third-party C++ rules) assign `CppLink` actions to the `"cpp_link"`
  execution group (`exec_group`).
- `CppCompile` actions and other actions run in the default execution group.

To route `CppLink` actions to large RBE workers based on the **target toolchain**:
- An execution platform variant `//build/bazel/platforms:linux_x64_large_cpp_link`
  is defined in `//build/bazel/platforms/BUILD.bazel`. It inherits from
  `//build/bazel/platforms:linux_x64` and prefixes large worker execution properties
  with `"cpp_link."` (e.g. `"cpp_link.Pool": "large_worker"`). Any action in the
  `"cpp_link"` execution group uses the large worker pool, while `CppCompile` and
  other default-group actions remain on the standard worker pool.
- In `//build/bazel/config/bazel_args.gni` (evaluated independently for the
  `fuchsia_default_variant_toolchain` and `host_default_variant_toolchain`):
  - When `_has_lto` is true for that toolchain, `--extra_execution_platforms=//build/bazel/platforms:linux_x64_large_cpp_link`
    is added to the platform's `flags`.
  - When `_has_lto` is false, `--extra_execution_platforms=` is explicitly added
    to reset the execution platform across transitions (including implicit `cfg = "exec"`
    host tool transitions).
- In `//build/bazel/templates/template.bazelrc`,
  `--experimental_override_name_platform_in_output_dir=//build/bazel/platforms:linux_x64_large_cpp_link=linux_x64`
  ensures that any host tools built from targets using `:linux_x64_large_cpp_link`
  share the canonical `linux_x64` output directory path prefix.

### Routing Rust Link Actions (`rustc_binary`, `rustc_test`, `rustc_dylib`) to Large RBE Workers in Bazel

Unlike `@rules_cc`, `@rules_rust` does not define a separate `rust_link` execution group:
- `rust_library` targets only compile `.rs` source code into `.rlib` archives without
  invoking the linker, and therefore do not require large workers.
- `rust_binary`, `rust_test`, and `rust_dylib_library` perform crate compilation and
  LTO linking together in a single `Rustc` action in the default execution group.

To route linked Rust actions to large RBE workers based on the **target configuration**:
- `//build/config/lto/BUILD.bazel` defines a configuration setting group
  `//build/config/lto:rust_lto_enabled` that matches when either:
  1. `//build/bazel/config/toolchain_tags/lto:true` is active, or
  2. An LTO/ThinLTO optimization mode (`moderate_lto`, `size_lto`, `speed_lto`,
     `moderate_thinlto`, `size_thinlto`, `speed_thinlto`) is active on a
     non-instrumented toolchain.
- `with_fuchsia_rust_link_exec_properties()` in `//build/bazel/rules/rust/common.bzl`
  applies `RBE_USE_LARGE_WORKER` (`build_config.rbe_large_exec_properties`) via
  `select({ "//build/config/lto:rust_lto_enabled": RBE_USE_LARGE_WORKER, "//conditions:default": {} })`.
- This helper is applied to the target-level `exec_properties` attribute of:
  - `rustc_binary` (`//build/bazel/rules/rust/rustc_binary.bzl`)
  - `rustc_dylib` (`//build/bazel/rules/rust/rustc_dylib.bzl`)
  - `rustc_test` (`//build/bazel/rules/rust/rustc_test.bzl`)
  - Implicit test targets created by `generate_unit_tests` (`//build/bazel/rules/rust/generate_unit_tests.bzl`)
