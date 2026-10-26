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

To route `CppLink` actions to large RBE workers:
1. In `//build/bazel/config/bazel_args.gni` (evaluated independently for the
   Fuchsia default toolchain and the host default toolchain), `_has_lto` is
   computed as `true` when `"lto"` is in `toolchain_variant.tags` or `optimize`
   is an LTO/ThinLTO mode. Its value is written as `has_lto` to
   `$BUILD_DIR/bazel_args/{fuchsia,host}.json`.
2. In `//build/bazel/scripts/workspace_utils.py`, `GnBuildArgs.generate_platforms_flags_bzl`
   reads `has_lto` from both JSON files and exports `fuchsia_has_lto` and `host_has_lto`
   as booleans in `@fuchsia_build_info//:platforms_flags.bzl` (with `False` default
   fallbacks in `//build/bazel/scripts/fake_build_info/platforms_flags.bzl` for
   `minimal_workspace.py` / `fx rbe auth`).
3. In `//build/bazel/platforms/BUILD.bazel`:
   ```python
   load("@fuchsia_build_info//:platforms_flags.bzl", "fuchsia_has_lto", "host_has_lto", "platform_flags_map")

   _linux_x64_exec_properties = build_config.rbe_remote_exec_properties | (
       {
           "cpp_link." + k: v
           for k, v in build_config.rbe_large_exec_properties.items()
       } if (fuchsia_has_lto or host_has_lto) else {}
   )

   platform(
       name = "linux_x64",
       ...
       exec_properties = _linux_x64_exec_properties,
   )
   ```
   Prefixing execution property keys with `"cpp_link."` (e.g. `"cpp_link.Pool": "large_worker"`)
   instructs Bazel to apply those properties specifically to actions in the `"cpp_link"`
   execution group, routing `CppLink` actions to the large RBE worker pool while keeping
   `CppCompile` and other default-group actions on standard workers.

### Routing Rust Link Actions (`rustc_binary`, `rustc_test`, `rustc_dylib`, `rustc_proc_macro`) to Large RBE Workers in Bazel

Unlike `@rules_cc`, `@rules_rust` does not define a separate `rust_link` execution group:
- `rust_library` targets only compile `.rs` source code into `.rlib` archives without
  invoking the linker, and therefore do not require large workers.
- `rust_binary`, `rust_test`, `rust_dylib_library`, and `rust_proc_macro` perform crate
  compilation and LTO linking together in a single `Rustc` action in the default execution group.

To route linked Rust actions to large RBE workers based on the target configuration:
1. `//build/config/lto/BUILD.bazel` defines a configuration setting group
   `//build/config/lto:rust_lto_enabled` that matches when either:
   - `//build/bazel/config/toolchain_tags/lto:true` is active, or
   - An `optimize` mode that enables LTO/ThinLTO for Rust (`moderate_lto`, `size_lto`,
     `speed_lto`, `moderate_thinlto`, `size_thinlto`, `speed_thinlto`) is active on a
     non-instrumented toolchain (excluding `size_thinlto_clang`, which only enables ThinLTO
     for Clang).
2. `with_fuchsia_rust_link_exec_properties()` in `//build/bazel/rules/rust/common.bzl`
   applies `RBE_USE_LARGE_WORKER` (`build_config.rbe_large_exec_properties` from
   `//build/bazel/rbe:rbe.bzl`) via:
   ```python
   select({
       "//build/config/lto:rust_lto_enabled": RBE_USE_LARGE_WORKER,
       "//conditions:default": {},
   })
   ```
3. This helper is applied to the target-level `exec_properties` attribute of Fuchsia's
   in-tree Rust wrapper macros:
   - `rustc_binary` (`//build/bazel/rules/rust/rustc_binary.bzl`)
   - `rustc_dylib` (`//build/bazel/rules/rust/rustc_dylib.bzl`)
   - `rustc_proc_macro` (`//build/bazel/rules/rust/rustc_proc_macro.bzl`)
   - `rustc_test` (`//build/bazel/rules/rust/rustc_test.bzl`)
   - Implicit unit test targets created by `generate_unit_tests` (`//build/bazel/rules/rust/generate_unit_tests.bzl`)

### Limitations and Trade-offs

This design is intentionally kept simple to facilitate review, ensure stability, and
avoid complex build-graph interactions (such as the `ActionConflictException` that
arises when registering multiple execution platforms with distinct labels for host
and Fuchsia targets). However, it introduces important trade-offs:

1. **Fuchsia / Host Coupling for C++ `CppLink` Actions**:
   Bazel reads `exec_properties` from the **execution platform**
   (`--host_platform=//build/bazel/platforms:host`, resolving to
   `//build/bazel/platforms:linux_x64`), rather than the **target platform**
   (`--platforms=...`). Furthermore, Bazel evaluates registered execution platforms
   in a constant empty configuration, meaning `select()` expressions on build flags
   cannot be used dynamically within `platform(exec_properties = ...)`.

   Because `:linux_x64` executes actions for both Fuchsia targets and host tools,
   if **any** of the Fuchsia or Host build configurations enables LTO
   (`fuchsia_has_lto or host_has_lto`), **all** C++ `CppLink` actions across both
   Fuchsia and Host builds are sent to the large RBE worker pool.

2. **In-Tree Wrapper Macro Scope for Rust Link Actions**:
   Because `@rules_rust` does not define a separate `rust_link` execution group, Rust
   link routing cannot be configured globally on the execution platform. Instead, it
   is applied per-target via `with_fuchsia_rust_link_exec_properties()` within Fuchsia's
   in-tree wrapper macros (`rustc_binary`, `rustc_test`, `rustc_dylib`, `rustc_proc_macro`,
   and generated unit tests). Linked Rust targets that directly invoke `@rules_rust`
   definitions without going through these macros will not receive large worker routing.
