# Fuchsia SDK Sysroot Generation

This directory defines the build rules and metadata generation for the Fuchsia
SDK sysroot atom (`pkg/sysroot`), distributed as part of the Fuchsia IDK and
used by external build systems (including Bazel via `@fuchsia_sdk`).

---

## Overview

### SDK Sysroot vs. Platform Sysroot

In Fuchsia, there are two distinct concepts of a "sysroot":

1. **In-Tree Platform Sysroot** (`//zircon/public/sysroot`):
   A toolchain-internal directory structure used exclusively during in-tree
   GN/Ninja compilation of C, C++, Rust, and Go code targeting Fuchsia. It is
   specific to the current compilation toolchain variant and target architecture.

2. **SDK / IDK Sysroot** (`//zircon/public/sysroot_sdk`):
   The canonical public sysroot distributed to out-of-tree developers and Bazel.
   It contains public headers, link-time stubs, startup objects, runtime dynamic
   linkers, and debug symbols spanning:
   - **All supported CPU architectures** (`arm64`, `riscv64`, `x64`).
   - **All supported API levels** (e.g. `27`, `28`, ..., `NEXT`).
   - **Supported sanitizer variants** (e.g. `asan`, and `hwasan` on `arm64`).

The SDK sysroot adheres to the schema defined in
`//build/sdk/meta/sysroot.json`.

---

## Architecture: Top-Level GN Generation vs. Sub-Builds

### Historical Model (Recursive Sub-Builds)

Historically, building the public IDK required running dozens of recursive
GN/Ninja sub-builds via `build/subbuild.py`. Each sub-build executed an
independent `gn gen` and Ninja build with a specialized `args.gn` targeting a
single `(target_cpu, api_level)` pair.

Each sub-build produced a partial `pkg/sysroot/meta.json` containing exactly one
`variant` (or one `version` for "PLATFORM"). The top-level build then invoked
`//build/sdk/generate_idk` to merge these partial manifests together.

### Current Model (Top-Level Multi-CPU & Multi-API Sysroot Generation)

In the current model, the top-level GN build generates all sysroot binaries,
headers, and metadata directly in a single graph without sub-builds:

1. **Multi-CPU Compilation**:
   A single top-level Ninja invocation compiles the C library (`libc.so`),
   startup code (`Scrt1.o`), and linker stubs (`libc.so`, `libzircon.so`) for
   `arm64`, `riscv64`, and `x64` concurrently.

2. **Unified IDK Export Action**:
   The action `//zircon/public/sysroot_sdk:sysroot_sdk_export` calls
   `//build/sdk/generate_sysroot_idk_atom.py`. It collects the compiled
   per-CPU binaries and common headers, synthesizes the complete multi-CPU and
   multi-API-level `pkg/sysroot/meta.json`, populates `arch/<cpu>/sysroot/` and
   `obj/<cpu>-api-<level>/sysroot/` using relative symlinks, and validates the
   result against `//build/sdk/meta/sysroot.json`.

Note: While `//zircon/public/sysroot_sdk:sysroot_sdk_export` generates the
complete sysroot atom from the top-level build (and `//build/sdk/generate_idk`
supports merging multi-version/multi-variant sysroot atoms), wiring IDK
sub-builds and the final IDK export to consume it directly is performed in
follow-up changes. Recursive GN/Ninja sub-builds remain in use for non-sysroot
prebuilt libraries (such as `libfdio.so`) and SDK packages, which genuinely
depend on both the target CPU architecture and the target API level.

---

## Subtleties & Design Decisions

### 1. API-Level Invariance of Sysroot Binaries & Headers

A critical finding that enables single-pass sysroot generation is that **Zircon
headers, the C library, dynamic loader, startup code, and linker stubs contain
zero source-level dependencies on `__Fuchsia_API_level__`**:

- Unlike high-level FIDL client libraries or SDK packages, `libc.so` and
  `libzircon.so` provide a stable system ABI that does not change between
  numbered API levels supported by a given platform release.
- In the historical sub-build IDK, the files in `obj/<cpu>-api-27/sysroot/`,
  `obj/<cpu>-api-28/sysroot/`, etc., were byte-for-byte identical duplicates of
  the base CPU sysroot files.

Because the compiled artifacts do not vary with the API level, defining separate
GN toolchains for every `(cpu x api_level)` pair (which would result in 15 to 45
toolchains) would cause massive redundant compilation of libc with zero output
difference. Instead, libc is compiled once per CPU, and the export tool
populates the multi-API-level directory layout and metadata variants.

### 2. Decoupled Multi-CPU Targets

GN does not support user-defined functions that return values, and GN scopes
cannot use dynamic strings as variable names (e.g. `scope."$cpu" = ...` is a
syntax error).

To support multi-CPU compilation without destabilizing the in-tree platform
sysroot:
- `sdk/lib/c/sysroot_entries.gni` remains focused on the in-tree sysroot.
- `zircon/public/sysroot_sdk/BUILD.gn` defines `sysroot_cpu_artifacts`, which
  explicitly loops over `standard_fuchsia_cpus` and references the necessary
  cross-compilation toolchains:
  - Startup code: `//sdk/lib/c/startup:startfiles(//build/toolchain/zircon:user.basic_${cpu})`
  - Stripped and debug libc: `//sdk/lib/c:libc(//sdk/lib/c:user.libc_${cpu})`
  - Sanitizer variants: `//sdk/lib/c:libc(//sdk/lib/c:user.libc_${cpu}-asan)`, etc.
  - Linker stubs: `//sdk/lib/c:c.stub.${cpu}`, `//src/zircon/lib/zircon:zircon.stub.${cpu}`

### 3. Toolchain Environment Gates in libc

Previously, `sdk/lib/c/BUILD.gn` and `zircon/third_party/ulib/musl/BUILD.gn`
contained hardcoded checks against `target_cpu`:
```gn
# Old check:
toolchain_variant.base == "//sdk/lib/c:user.libc_$target_cpu"
```
In an `x64` host build, `target_cpu` is `x64`. This prevented the GN graph from
instantiating `user.libc_arm64` or `user.libc_riscv64`.

This was modernized to check:
```gn
toolchain_environment == "user.libc"
```
and matching against `current_cpu` rather than `target_cpu`. Furthermore, when
already inside a `user.libc` toolchain for a secondary architecture,
`libc_toolchain` resolves directly to `current_toolchain` rather than attempting
to re-resolve via `target_cpu`.

Additionally, in `libc_config` (`sdk/lib/c/BUILD.gn`), checking the global
`is_fuchsia_hwasan` flag was replaced with a per-toolchain check against
`toolchain_variant.tags` (`toolchain_variant.tags + [ "hwasan" ] - [ "hwasan" ] != toolchain_variant.tags`).
On `arm64-hwasan` builders where `is_fuchsia_hwasan` is globally `true`, relying
on the global flag caused non-hwasan `user.libc` toolchains (such as
`user.libc_arm64` and `user.libc_arm64-asan`) to pass `-nostdlib++` without
`-fsanitize=hwaddress`, resulting in undefined `std::__libcpp_verbose_abort`
linker errors.

### 4. IFS Shared Library Link Stubs

Linker stubs for `libc.so` and `libzircon.so` are generated using LLVM IFS
(`llvm-ifs`). The template `//build/toolchain/ifs_shared_library.gni` defines:
- `$stub_target.$cpu` (e.g. `//sdk/lib/c:c.stub.arm64`)
- `$stub_target.all_cpus`

This allows sysroot targets to take a direct dependency on any architecture's
stubs without requiring cross-toolchain GN hops for stub generation.

### 5. Sanitizer Variant Differences

The supported sanitizers vary by architecture:
- `arm64`: Supports both `asan` and `hwasan` (Hardware-assisted AddressSanitizer).
- `x64` and `riscv64`: Support `asan`, but not `hwasan`.

The generator script and GN manifests dynamically include `hwasan` binaries
(`dist/lib/hwasan/ld.so.1` and `debug/hwasan/libc.so`) only for `arm64`.

Note that while `user.libc_${cpu}` toolchains in `sdk/lib/c/BUILD.gn` enable
additional variants (`asan-ubsan`, `ubsan`, `coverage`, and `hwasan-ubsan` on
`arm64`) for the in-tree platform sysroot on `target_cpu`, the SDK/IDK sysroot
(`sysroot_cpu_artifacts`) only builds and exports the base variant, `asan`, and
`hwasan` (on `arm64`).

### 6. Linker Redirection Scripts

Standard POSIX libraries like `-lm`, `-ldl`, `-lpthread`, and `-lrt` are all
implemented directly within `libc.so` in Fuchsia.

To satisfy build systems and compiler drivers expecting `-lm` or `-lpthread`,
the sysroot includes linker scripts (`libm.so`, `libdl.so`, `libpthread.so`,
`librt.so`) copying `//zircon/third_party/ulib/musl/lib.ld`:
```text
/* Redirect linker to libc.so */
INPUT(AS_NEEDED(libc.so))
```

### 7. Schema Invariant: `dist_dir` vs. `sysroot_dir`

The metadata schema `//build/sdk/meta/sysroot.json` imposes a subtle schema
difference between `versions` and `variants`:
- In `versions.<cpu>` (API-agnostic prebuilts):
  - **Required**: `dist_dir` (pointing to `arch/<cpu>/sysroot`)
  - **Forbidden**: `sysroot_dir`
- In `variants[].values` (per-API-level prebuilts):
  - **Required**: `sysroot_dir` (pointing to `obj/<cpu>-api-<level>/sysroot`)
  - **Forbidden**: `dist_dir`

`generate_sysroot_idk_atom.py` strictly adheres to this schema distinction, and
validates every generated `meta.json` using `json_validator_valico` at build
time.

---

## Key Files & Targets

- **`zircon/public/sysroot_sdk/BUILD.gn`**:
  Defines `sysroot_artifacts_all_cpus`, `sysroot_manifest_inputs`, and the
  export action `sysroot_sdk_export`.
- **`build/sdk/generate_sysroot_idk_atom.py`**:
  Python action script that constructs `pkg/sysroot/meta.json` and creates
  relative symlinks for all sysroot files.
- **`build/sdk/meta/sysroot.json`**:
  JSON schema defining the canonical structure of the sysroot SDK atom.
- **`build/toolchain/ifs_shared_library.gni`**:
  Defines per-CPU IFS stub targets.
- **`sdk/lib/c/BUILD.gn`** and **`zircon/third_party/ulib/musl/BUILD.gn`**:
  Defines the multi-CPU C library compilation rules and toolchain environments.
- **`build/sdk/generate_idk/__init__.py`**:
  IDK merger module capable of merging multi-variant and multi-version sysroot
  atoms.
