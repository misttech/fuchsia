<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# C++ PIE vs PIC in GN and Bazel (Fuchsia and Host Linux)

This document explains how position-independent code (PIC) for shared libraries
and position-independent executable code (PIE) for executables and their static
library dependencies are modeled in both GN and Bazel in the Fuchsia build.

---

## 1. Background: `-fPIC` vs `-fPIE` on ELF Platforms

Fuchsia binaries targeting user space and Linux host binaries run as ELF binaries:

- **ELF Shared Libraries** (`ET_DYN` with shared object semantics):
  Shared libraries (`.so`) can be loaded at arbitrary virtual addresses and
  allow symbol interposition. Machine code for shared libraries must be compiled
  with `-fPIC` (Position-Independent Code).

- **ELF Executables** (`ET_DYN` with `DF_1_PIE`):
  Both Fuchsia and Linux host executables are Position-Independent Executables (PIE)
  to support Address Space Layout Randomization (ASLR). Unlike shared libraries,
  executables cannot have their symbols interposed at runtime. Compiling code
  for an executable with `-fPIE` (Position-Independent Executable) allows the
  compiler to assume that intra-module global variables and functions defined in
  the same binary will not be preempted, generating direct PC-relative references
  rather than going through the Global Offset Table (GOT). This produces smaller,
  faster code and avoids indirect memory lookups.

Static libraries (`cc_library` / `fx_cc_library` / `static_library` /
`source_set`) may be linked into either an executable or a shared library. When
linked into an executable, compiling them with `-fPIE` yields optimal
performance and size; when linked into a shared library, they must be compiled
with `-fPIC`.

### Clang Defaults on Linux

Prebuilt Clang targeting Linux (`x86_64-unknown-linux-gnu`) is built with
`CLANG_DEFAULT_PIE_ON_LINUX=ON`. Consequently, when no `-fpic`, `-fPIC`,
`-fpie`, `-fPIE`, `-fno-pic`, or `-fno-pie` flag is explicitly provided, Clang
defaults to `-fPIE` code generation.

---

## 2. How GN Handles `-fPIE` vs `-fPIC`

GN models the separation between PIC and PIE differently for Fuchsia and host Linux:

### Fuchsia Targets: Dual Companion Toolchains

1. **Base Toolchain** (e.g. `//build/toolchain/fuchsia:x64`):
   - Defined with `is_pic_default = false`.
   - `default_executable_configs` contains `//build/config:executable_config`,
     which passes `cflags = [ "-fPIE" ]`, `asmflags = [ "-fPIE" ]`, and
     `ldflags = [ "-fPIE" ]`.
   - `default_common_binary_configs` also includes `//build/config:executable_config`
     on Fuchsia when `is_pic_default` is `false`, ensuring that `source_set` and
     `static_library` targets compiled in this toolchain are built with `-fPIE`.
   - `shared_library()` targets defined in the base toolchain redirect to the
     companion `-shared` toolchain.

2. **Companion Shared Library Toolchain** (e.g. `//build/toolchain/fuchsia:x64-shared`):
   - Defined with `is_pic_default = true`.
   - `default_common_binary_configs` includes `//build/config:shared_library_config`,
     which passes `cflags = [ "-fPIC" ]`, `asmflags = [ "-fPIC" ]`, and
     `ldflags = [ "-fPIC" ]`.
   - All `shared_library` targets and their transitive `source_set` and
     `static_library` dependencies are instantiated in this toolchain and
     compiled with `-fPIC`.

When both an `executable()` and a `shared_library()` depend on the same
`static_library()`, GN instantiates the static library twice - once in each
toolchain - compiling two separate sets of object files.

### Linux Host Targets: Single Toolchain

On Linux host, GN uses a single `host_x64` toolchain without a companion `-shared`
toolchain (`with_shared = false`).

- Historically, `//build/config/linux:default-pie` was included in
  `default_common_binary_configs`, passing `-fPIE` to all host targets.
- In December 2024, `default-pie` was moved from `default_common_binary_configs`
  to `default_executable_configs` only. This ensured that host `executable` and
  `rustc_binary` targets receive explicit `-fPIE` and `-pie` flags.
- Host `static_library` and `source_set` targets do not receive an explicit
  `-fPIE` or `-fPIC` flag in GN, relying on Clang's built-in
  `CLANG_DEFAULT_PIE_ON_LINUX=ON` default.

---

## 3. Rust Interoperability and Requirements

Fuchsia binaries and build tools frequently mix Rust and C/C++ dependencies:

1. **Rust Compiler Defaults**:
   The Rust compiler (`rustc`) defaults to `-C relocation-model=pic` on ELF
   targets.

2. **Rust Binaries (`bin` crate type)**:
   - When building an executable binary, `rustc` invokes the linker with `-pie`
     (or `-Clink-arg=-pie`).
   - A Rust binary can safely link C/C++ static libraries compiled with either
     `-fPIE` or `-fPIC`. Compiling C/C++ dependencies with `-fPIE` produces
     more efficient code by avoiding GOT indirection for non-preemptible symbols.

3. **Rust Dynamic Libraries (`dylib`, `cdylib`, `proc-macro`)**:
   - When building a dynamic library, `rustc` invokes the linker with `-shared`.
   - Dynamic libraries require all linked code to be position-independent with
     support for symbol interposition, which necessitates `-fPIC`. Linking
     code compiled with `-fPIE` into a shared library can fail on certain
     architectures or link scenarios because `-fPIE` allows the compiler to omit
     PLT/GOT entries that a shared library requires.

4. **GN Limitation vs Bazel Solution**:
   - In GN, because the Linux host toolchain lacks a `-shared` companion toolchain,
     C/C++ static dependencies linked into host Rust proc-macros or dylibs were
     compiled without explicit `-fPIC` (falling back to implicit `-fPIE`).
   - In Bazel, `@rules_cc`'s `supports_pic` mechanism generates **both**
     `*.o` (`-fPIE`) and `*.pic.o` (`-fPIC`) for any `cc_library` target. When
     `@rules_rust` or `@rules_cc` links an executable, it consumes `*.o` (`-fPIE`);
     when linking a dynamic library, cdylib, or proc-macro, Bazel automatically
     consumes `*.pic.o` (`-fPIC`). This provides clean, correct separation
     without relying on toolchain duplication.

---

## 4. Why Starlark Configuration Transitions Are Not Used in Bazel

In Bazel, one might initially consider introducing an incoming Starlark
configuration transition on `fx_cc_binary()` and `fx_cc_shared_library()` to
toggle a build setting specifying whether to build dependencies in PIC or PIE
mode.

However, using a configuration transition causes several significant problems:

1. **Build Graph Splitting & Duplication**:
   A configuration transition bifurcates the configured target graph. Any
   shared non-C++ dependencies (such as FIDL libraries, Rust libraries, data
   dependencies, or custom tools) would be analyzed and built twice under two
   distinct configuration hashes, causing substantial memory overhead and
   cache misses.

2. **Data Dependency Cycles**:
   Executables can be data dependencies of shared libraries (e.g. helper daemons
   packaged alongside runtime libraries), and shared libraries can be
   dependencies of executables. Transitions in both directions can lead to graph
   explosions or configuration mismatches.

3. **Incompatibility with `CcInfo` and `cc_shared_library`**:
   Bazel's native C++ rules and `cc_shared_library` expect dependencies to share
   compatible C++ toolchain configurations. Transitioning configurations on
   macro wrappers can break provider propagation and aspect evaluation.

4. **Aspect Hints & Tooling Friction**:
   Tools analyzing the build graph (such as IDE synchronizers, query tools, and
   code generators) struggle with transitioned target variants.

---

## 5. How Bazel and `@rules_cc` Handle `-fPIC` vs `-fPIE` Natively

Instead of using configuration transitions, Fuchsia relies on Bazel's native
`@rules_cc` mechanism for handling PIC vs non-PIC compilation within a single
configuration.

In `@rules_cc`, the C++ toolchain declares:
- `supports_pic`: A built-in feature indicating that the toolchain can produce
  position-independent objects.
- `pic`: A toolchain feature defining compiler flags when position-independent
  compilation is active.

Upstream `@rules_cc` normally forces `use_pic_for_binaries = True` whenever
`compilation_mode != "opt"`, which would cause `dbg` and `fastbuild` modes to
compile executables and static libraries with `-fPIC` instead of `-fPIE`. On
Fuchsia and Linux host, executables and their static library dependencies are
compiled with `-fPIE` across all compilation modes, matching GN.

To achieve this, Fuchsia applies a local patch
(`//build/bazel/vendor_patches/rules_cc-0.2.19/0001-rules_cc-support-pie-executables-in-dbg-and-fastbuild.patch`)
to `@rules_cc` that removes the `compilation_mode != "opt"` forcing condition.
As a result:
- `cc_common.use_pic_for_binaries` is `False` across all compilation modes
  (`dbg`, `fastbuild`, `opt`).
- Sources in `cc_library` are compiled **twice** across all compilation modes:
  1. Once with the `pic` build variable set, generating `*.pic.o` (packaged
     into `lib<name>.pic.a`), used when linking dynamic libraries
     (`cc_binary(..., linkshared = True)` / `fx_cc_binary(..., linkshared = True)`
     and `cc_shared_library` / `fx_cc_shared_library`), as well as Rust
     `dylib`, `cdylib`, and `proc-macro` targets.
  2. Once without the `pic` build variable set, generating `*.o` (packaged
     into `lib<name>.a`), used when linking executables (`cc_binary` /
     `fx_cc_binary`) and Rust `bin` targets.
- `cc_binary(..., linkshared = True)` / `fx_cc_binary(..., linkshared = True)`
  links its own `srcs` as `*.pic.o` (`-fPIC`) and links with `-fPIC` (while
  `@rules_cc` registers both `*.pic.o` and `*.o` compile actions in the action
  graph, only `*.pic.o` is requested by the link action and executed).
- `cc_shared_library` / `fx_cc_shared_library` links dynamic libraries from
  exported `cc_library` dependencies with `-fPIC`.
- `cc_binary` / `fx_cc_binary` (executable) links its own `srcs` as `*.o`
  (`-fPIE`) and links with `-fPIE` (and `-pie` on Linux host).

---

## 6. Why `feature(name = "pic")` Must Emit `-fPIC` / `-fPIE`

Fuchsia's `fx_cc_library()` macro attaches build flags using the
`wrap_cc_macro_args_with_build_flags` helper, which generates response files
passed via `copts = ["@$(location <name>.compile_common.build_flags)"]`.

However:
- Both the `*.pic.o` action and the `*.o` action of a given `fx_cc_library()`
  target share the exact same `copts` response file across all compilation modes.
- If `-fPIC` or `-fPIE` were emitted in `build_flags()` response files, both
  actions would receive the same flag, conflicting with the desired separation.

Therefore:
- `//build/config/fuchsia:shared_library_config`,
  `//build/config/fuchsia:executable_config`, and
  `//build/config:shared_library_config` do not emit `-fPIC` or `-fPIE` directly.
- The C++ toolchain in
  `//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl`
  defines `feature(name = "pic")`:
  ```python
  pic_feature = feature(
      name = "pic",
      enabled = is_linux or is_fuchsia,
      flag_sets = [
          flag_set(
              actions = action_names.all_compile_actions,
              flag_groups = [
                  flag_group(
                      flags = ["-fPIC"],
                      expand_if_available = "pic",
                  ),
                  flag_group(
                      flags = ["-fPIE"],
                      expand_if_not_available = "pic",
                  ),
              ],
          ),
          flag_set(
              actions = [
                  ACTION_NAMES.cpp_link_dynamic_library,
                  ACTION_NAMES.cpp_link_nodeps_dynamic_library,
              ],
              flag_groups = [
                  flag_group(flags = ["-fPIC"]),
              ],
          ),
          flag_set(
              actions = [ACTION_NAMES.cpp_link_executable],
              flag_groups = [
                  flag_group(
                      flags = ["-fPIE"] + (["-pie"] if is_linux else []),
                  ),
              ],
          ),
      ],
  )
  ```
  - When `@rules_cc` compiles a `*.pic.o` object, the `pic` build variable is
    available, emitting `-fPIC`.
  - When compiling a `*.o` object for an executable or static library, `pic` is
    not available, emitting `-fPIE`.
  - When linking dynamic libraries, `-fPIC` is emitted.
  - When linking executables, `-fPIE` is emitted. On Linux host, `-pie` is also
    emitted to tell the host linker to generate a position-independent
    executable (on Fuchsia, the driver and sysroot produce PIE executables by
    default).

---

## 7. How `beads` Verifies `-fPIE` vs `-fPIC`

`beads` verifies build command parity between Ninja (GN) and Bazel actions:

1. **Compile Action Selection**:
   `bazel aquery` returns two `CppCompile` actions for an `fx_cc_library`
   target (`*.pic.o` and `*.o`).
   `//build/beads/scripts/compare_utils.py` filters compile actions directly by
   `use_pic`:
   - When comparing against a GN target in a `-shared` toolchain
     (e.g. `//...(//build/toolchain/fuchsia:x64-shared)`), `query.bazel.use_pic`
     is `True`, so `compare_utils.py` selects the `*.pic.o` action (`-fPIC`).
   - When comparing against a GN target in the default toolchain (or Linux host),
     `query.bazel.use_pic` is `False`, so `compare_utils.py` selects the `*.o`
     action (`-fPIE`).

2. **Command Map Keys**:
   `ConfiguredBazelLabel` in `compare_utils.py` includes `use_pic: bool = False`
   to allow storing both the non-PIC and PIC compilation commands for the same
   Bazel target label in the command map without collision.

3. **Link Actions & Rust Parity**:
   - `beads` does **not** need code modifications for Rust binary link actions:
     `//build/beads/scripts/normalize_rustc_args.py` intentionally ignores
     `-Clink-arg=` and `-Clink-args=` when comparing `rustc` command lines.
   - For C++ host executables (`beads_cc_example_bin`), Bazel now emits `-fPIE -pie`
     matching GN's `//build/config/linux:default-pie`.
