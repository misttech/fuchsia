---
name: port-gn-config-to-bazel-build-flags
description: >
  Guides porting Fuchsia GN config() and compiler_config() targets to Bazel
  build_flags() targets (both default toolchain configs and non-default
  configs across //build/config/**/BUILD.gn and other packages). Use when
  creating build_flags() definitions, updating corresponding out-of-tree (OOT)
  flags in cc_features.bzl / default_flag_values.bzl, exposing global GN build
  variables via gn_build_variables_for_bazel or
  //build/bazel/config/derived/*.bzl, mapping OS/CPU conditions to Bazel
  select() expressions, or auditing config() definitions for GN/Ninja-specific
  limitations.
---

# Porting GN `config()` Definitions to Bazel `build_flags()`

This skill describes how to port Fuchsia GN `config()` and `compiler_config()`
target definitions into equivalent Bazel `build_flags()` targets. It applies to:

- **Default toolchain configs** listed as missing in
  `@fuchsia_build_info//default_build_flags/BUILD.bazel`
  (`bazel-repos/fuchsia_build_info+/default_build_flags/BUILD.bazel`).
- **Non-default `config()` targets** defined in `//build/config/**/BUILD.gn` or
  any other package in the source tree that individual targets reference via
  `configs += [...]` / `build_flags = [...]`.

---

## 1. Background: `build_flags()` and Default Config Discovery

### The `build_flags()` Rule
Defined in `//build/bazel_sdk/fuchsia_rules_common/build_flags/build_flags.bzl`,
`build_flags()` exposes compiler and linker flags to C/C++ and Rust rules via
`BuildFlagsInfo`. Its attributes are configurable (`attr.string_list` /
`attr.label_list`), meaning **they support both Starlark constants and Bazel
`select()` expressions**:

| GN `config()` / `compiler_config()` Attribute | Bazel `build_flags()` Attribute |
|---|---|
| `cflags` | `cflags` |
| `cflags_c` | `cflags_c` |
| `cflags_cc` | `cflags_cc` |
| `defines` | `defines` |
| `include_dirs` | `include_dirs` |
| `ldflags` | `ldflags` |
| `lib_dirs` | `lib_dirs` |
| `rustenv` | `rustenv` |
| `rustflags` | `rustflags` |
| `configs` / `compiler_configs` | `subflags` |
| `c_family_flags` (in `compiler_config()`) | Expands to `cflags` and `ldflags` |

*(Note: `cflags_objc`, `cflags_objcc`, and `asmflags` are not separate
attributes on `build_flags()`.)*

### Default Toolchain Config Discovery
During `fx gen` / `fx build`, `//build/bazel/config:bazel_default_configs_json`
writes the default Fuchsia and Host config lists to
`$BUILD_DIR/bazel_default_configs/{fuchsia,host}.json`. The script
`//build/bazel/scripts/bazel_build_flags.py` checks whether each GN label
`//<package>:<name>` has a matching `build_flags(name = "<name>", ...)` target
in `//<package>/BUILD.bazel`, and generates
`@fuchsia_build_info//default_build_flags/BUILD.bazel` with any unported configs
listed in a trailing comment.

---

## 2. Step 1: Classify GN Variables Referenced by the `config()`

Inspect the `config()` / `compiler_config()` body and all transitive sub-configs
to classify every GN variable it depends on into one of four categories:

### Category 1: Bazel Invariant Constants (Evaluate Statically)

In Bazel builds, the following GN variables have fixed invariant values that are
independent of the target build configuration:

| GN Variable | Bazel Invariant Value | Notes / Implied Values |
|---|---|---|
| `is_gcc` | `false` | Implies `is_clang = true`, `linker = "lld"` (in `//build/config/linker.gni`), and `default_compress_debuginfo = "zstd"` (in `//build/config/compiler.gni`). |
| `is_pecoff` | `false` | Implies `is_win = false` and `is_uefi = false`. |
| `is_kernel` | `false` | Kernel builds do not use Bazel `build_flags()`. |
| `zircon_toolchain` | `false` | Zircon-specific toolchains (`zircon_toolchain != false`) are not used in Bazel `build_flags()`. |
| `is_elf` | `true` | Both Fuchsia and Linux host targets use ELF. |
| `is_dwarf` | `true` | Both Fuchsia and Linux host targets use DWARF. |

**Mandatory Comment Rule**: Whenever you simplify a GN `if (...)` branch using
one or more of these invariants, add a comment directly above the
`build_flags()` definition stating the assumed invariant(s), for example:

```python
# NOTE: This assumes is_gcc=false && is_pecoff=false in Bazel.
```

or:

```python
# NOTE: This assumes is_kernel=false && zircon_toolchain=false in Bazel.
```

---

### Category 2: Global GN Variables (`declare_args()` & Derived Globals)

Variables whose values are constant across all GN toolchains in a given build
(e.g., `rust_incremental`, `rust_cap_lints`, `experimental_cxx_version`,
`use_ccache`, `deny_warnings`, `is_release`, `fuchsia_cxx_version`) should be
exposed as Starlark constants via `@fuchsia_build_info//:args.bzl` or
`//build/bazel/config/derived/*.bzl` (see **Step 2** below).

---

### Category 3: Target Platform, OS, and CPU Variables (Port via `select()`)

When a `config()` branches on target OS or CPU variables (`current_os`,
`current_cpu`, `is_fuchsia`, `is_linux`, `is_host`, `rust_target`), use Bazel
`select()` expressions on `build_flags()` attributes:

1.  **OS Conditions (`current_os`, `is_fuchsia`, `is_linux`, `is_host`)**:
    - `is_fuchsia` or `current_os == "fuchsia"`:

     ```python
     select({
         "@platforms//os:fuchsia": [...],
         "//conditions:default": [],
     })
     ```

    - `is_linux` or `current_os == "linux"`:

     ```python
     select({
         "@platforms//os:linux": [...],
         "//conditions:default": [],
     })
     ```

    - `is_host`: Use `//build/bazel/platforms:is_host_os` (defined in
      `//build/bazel/platforms/BUILD.bazel` using
      `build_config.host_platform_os_constraint`, matching `HOST_OS_CONSTRAINTS`
      from `//build/bazel/platforms:constraints.bzl`):

     ```python
     select({
         "//build/bazel/platforms:is_host_os": [...],
         "//conditions:default": [],
     })
     ```

2.  **CPU Conditions (`current_cpu`)**:
    - `current_cpu == "x64"` -> `"@platforms//cpu:x86_64"`
    - `current_cpu == "arm64"` -> `"@platforms//cpu:arm64"`
    - `current_cpu == "riscv64"` -> `"@platforms//cpu:riscv64"`

3.  **Combined `(current_os, current_cpu)` Conditions (e.g., `rust_target`)**:
    - `//build/bazel/platforms/BUILD.bazel` provides composite `config_setting`
      targets for every `(os, cpu)` pair:
      - `//build/bazel/platforms:is_fuchsia_x64`
      - `//build/bazel/platforms:is_fuchsia_arm64`
      - `//build/bazel/platforms:is_fuchsia_riscv64`
      - `//build/bazel/platforms:is_linux_x64`
      - `//build/bazel/platforms:is_linux_arm64`
    - Reusable `select()` dictionaries for platform-dependent flag lists can be
      placed in `//build/bazel/config/derived/<name>.bzl`. Because `select()` is
      not recognized in the top-level context of `.bzl` files (only in `BUILD`
      files or functions/macros evaluated during `BUILD` loading), define the
      top-level constant as a dictionary suffixed with `__select_args` to
      clarify that it must go into a `select()` statement at the call site. Note
      also that in Bazel Starlark, `select()` operates on list attributes
      (`list[str]`) rather than interpolating inside a string, so each
      dictionary value should be a flag list:

     ```python
     # //build/bazel/config/derived/rust_target.bzl
     rust_target_flags__select_args = {
         "//build/bazel/platforms:is_fuchsia_x64": ["--target", "x86_64-unknown-fuchsia"],
         "//build/bazel/platforms:is_fuchsia_arm64": ["--target", "aarch64-unknown-fuchsia"],
         "//build/bazel/platforms:is_fuchsia_riscv64": ["--target", "riscv64gc-unknown-fuchsia"],
         "//build/bazel/platforms:is_linux_x64": ["--target", "x86_64-unknown-linux-gnu"],
         "//build/bazel/platforms:is_linux_arm64": ["--target", "aarch64-unknown-linux-gnu"],
     }
     ```

      Then pass the dictionary to `select()` in `BUILD.bazel`:

     ```python
     load("//build/bazel/config/derived:rust_target.bzl", "rust_target_flags__select_args")

     build_flags(
         name = "target",
         rustflags = select(rust_target_flags__select_args),
         visibility = ["//visibility:public"],
     )
     ```

4.  **`compiler_config()` `linker_flags` Expansion**:
    - In `//build/config/compiler_config.gni`, setting `linker_flags = [ ... ]`
      adds `["-Wl," + ",".join(linker_flags)]` to `ldflags` and branches on `if
      (is_fuchsia)` when populating `rustflags`:
      - On Fuchsia (`is_fuchsia == true`): `["-Clink-arg=<flag>", ...]`
      - On Host (`is_fuchsia == false`): `["-Clink-arg=-Wl,<flag1>,<flag2>",
        ...]`
    - If a `compiler_config()` has non-empty `linker_flags`, its `rustflags` can
      either use `select({"@platforms//os:fuchsia": ..., "//conditions:default":
      ...})` (or if `select()` is avoided in purely global default flags, wait
      until platform-aware `select()` flags are enabled).

5.  **Toolchain-Specific GN Variables (`optimize`, `debuginfo`, `dwarf_version`,
    `compress_debuginfo`, `is_profile`)**:
    - Exposed as Bazel `build_setting()` flags in
      `//build/bazel/config/variables:<varname>` (populated by
      `//build/bazel/config/bazel_args.gni`) with matching `config_setting()`
      predicates in `//build/bazel/config/select/<varname>:<value>`.
    - See `//build/bazel/config/BUILD_CONFIGURATION_VARIABLES.md` for details.

6.  **GN Toolchain Variant Tags (`toolchain_variant.tags` and
    `toolchain_variant.instrumented`)**:
    - Exposed via `//build/bazel/config/toolchain_tags/<tag>:{true,false}`
      (where `toolchain_variant.instrumented` is equivalent to the
      `"instrumented"` tag:
      `//build/bazel/config/toolchain_tags/instrumented:true`).
    - For composite conditions (e.g. `toolchain_variant.instrumented ||
      is_lto`), use `selects.config_setting_group(match_any = [...])` from
      `@bazel_skylib//lib:selects.bzl`.
    - If a required tag from `//build/toolchain/variant_tags.gni` is not yet
      present under `//build/bazel/config/toolchain_tags/`, add it to
      `DEFAULT_TAGS_LIST` in `//build/bazel/config/toolchain_tags/update.py` and
      re-run the script.

---

### Category 4: GN / Ninja-Specific Limitations (Cannot Be Ported Directly)

Some `config()` definitions depend on low-level GN / Ninja mechanics that have
no direct Bazel equivalent and require a distinct Bazel-specific design:

1.  **Other `toolchain_variant` fields and `toolchain_environment`**:
    - Fields such as `toolchain_variant.is_pic_default` (e.g., in
      `//build/config/linux:default-pie`) or `toolchain_environment` are
      low-level details of how GN models companion `-shared` or environment
      toolchains.
    - Do not attempt to expose `toolchain_variant` through
      `gn_build_variables_for_bazel`.
2.  **GN / Ninja Output Directory Variables (`root_gen_dir`, `root_out_dir`,
    `root_build_dir`)**:
    - Configs such as `//build/config:default_include_dirs` (`include_dirs =
      ["//", root_gen_dir]`) or `//build/config/linux:sysroot`
      (`rebase_path(sysroot, root_build_dir)`) depend on Ninja's output
      directory layout and cannot be ported 1-to-1 to `build_flags()`.

---

## 3. Step 2: Expose Global GN Variables to Bazel

When a `config()` depends on a global GN variable (Category 2), expose it using
one of two patterns:

### Pattern A: Variable Defined in `declare_args()`

1.  **Ensure the `declare_args()` block is in a `.gni` file**:
    - `//build/bazel/BUILD.gn` cannot `import()` a `BUILD.gn` file. If the
      variable is declared in a `BUILD.gn` file (e.g.
      `//build/config/BUILD.gn`), move its declaration into a `.gni` file
      already imported by that `BUILD.gn` file (such as
      `//build/config/compiler.gni` or `//build/rust/config.gni`).
2.  **Export in `//build/bazel/BUILD.gn`**:
    - Add an entry to `generated_file("gn_build_variables_for_bazel")` in
      `//build/bazel/BUILD.gn` with `# LINT.IfChange` / `#
      LINT.ThenChange(<gni_file>)`:

     ```gn
     # LINT.IfChange
     declaration = "//build/toolchain/ccache.gni"
     import(declaration)
     contents += [
       {
         name = "use_ccache"
         value = use_ccache
         type = "bool"
         location = declaration
       },
     ]

     # LINT.ThenChange(//build/toolchain/ccache.gni)
     ```

    - Supported `type` values: `"bool"`, `"string"`, `"string_or_false"` (maps
      `false` to `""` and integers/strings to `"<value>"`),
      `"array_of_strings"`, and `"path"`.
3.  **Load in `BUILD.bazel`**:

    ```python
    load("@fuchsia_build_info//:args.bzl", "use_ccache")
    ```

### Pattern B: Variable Derived from Other Global Variables

1.  Export the underlying `declare_args()` variable(s) via
    `gn_build_variables_for_bazel` (Pattern A).
2.  Create `//build/bazel/config/derived/<variable_name>.bzl` duplicating the GN
    logic, with bidirectional `LINT.IfChange` / `LINT.ThenChange` annotations:

    ```python
    load("@fuchsia_build_info//:args.bzl", "experimental_cxx_version")

    # LINT.IfChange(fuchsia_cxx_version)
    _default_cxx_version = 23

    fuchsia_cxx_version = (
        int(experimental_cxx_version) if experimental_cxx_version else _default_cxx_version
    )
    # LINT.ThenChange(//build/config/fuchsia_cxx_version.gni:fuchsia_cxx_version)
    ```

3.  Load in `BUILD.bazel`:

    ```python
    load("//build/bazel/config/derived:fuchsia_cxx_version.bzl", "fuchsia_cxx_version")
    ```

---

## 4. Step 3: Author `build_flags()` & LINT Checks

Place the `build_flags()` definition in `<package>/BUILD.bazel` alongside
`<package>/BUILD.gn` with the exact same target name, and wrap both in
`LINT.IfChange(<name>)` / `LINT.ThenChange(...)`:

```python
# LINT.IfChange(no_rtti)
# NOTE: This assumes is_kernel=false && zircon_toolchain=false in Bazel.
build_flags(
    name = "no_rtti",
    cflags_cc = ["-fno-rtti"],
    ldflags = ["-fno-rtti"],
    visibility = ["//visibility:public"],
)
# LINT.ThenChange(//build/config/BUILD.gn:no_rtti)
```

---

## 5. Step 4: Update Out-of-Tree (OOT) Toolchain Flags for Default Configs

When porting a **default toolchain config** (one that becomes active in
`@fuchsia_build_info//default_build_flags/BUILD.bazel`), you **must** also
update the out-of-tree C++ toolchain flag definitions in
`//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/cc_features.bzl`
(and, when applicable, `toolchain_utils.bzl` or
`//build/bazel_sdk/fuchsia_rules_common/build_flags/default_flag_values.bzl`):

1.  **Dual-Mode Architecture (`use_default_build_flags`)**:
    - In-tree platform and host builds (`--config=fuchsia_platform`,
      `--config=host`) set
      `@fuchsia_rules_common//build_flags:use_default_build_flags = True` and
      receive default flags from `@fuchsia_build_info//default_build_flags`.
    - Out-of-tree (OOT) workspaces and SDK target builds
      (`--config=fuchsia_sdk`) use `@fuchsia_clang` or have
      `use_default_build_flags = False`, relying on
      `get_default_compile_flags_feature()` in `cc_features.bzl`.
    - Inside `get_default_compile_flags_feature()`, default flags are
      partitioned into:
      - `in_tree_remaining_{cflags,conlyflags,ccflags,ldflags}`: Flags not yet
        in `@fuchsia_build_info//default_build_flags` (applied unconditionally).
      - `oot_only_{cflags,conlyflags,ccflags,ldflags}`: Flags already in
        `@fuchsia_build_info//default_build_flags` (applied only when `not
        use_default_build_flags`).

2.  **Required Updates in `cc_features.bzl` When Adding a Default
    `build_flags()`**:
    - **Move existing flags from `in_tree_remaining_*` to `oot_only_*`** (or
      gate mode-specific flag sets on `not use_default_build_flags`) so they are
      not duplicated in-tree and can be removed via `disable_build_flags =
      [...]` on `fx_cc_*` targets.
    - **Add or update the OOT flag config in `_flag_configs` and `oot_only_*` to
      match the `//build/config/` equivalent evaluated for the `fuchsia_sdk`
      platform (`target_os == "fuchsia"`).** If the newly ported `build_flags()`
      was previously missing or outdated in `cc_features.bzl`, define or update
      its entry in `_flag_configs` and include it in the appropriate
      `oot_only_{cflags,conlyflags,ccflags,ldflags}` list.
    - **Share multi-flag lists via `default_flag_values.bzl`**: For configs with
      larger flag lists (such as `default_warnings` or
      `thread_safety_annotations`), define shared constants in
      `//build/bazel_sdk/fuchsia_rules_common/build_flags/default_flag_values.bzl`
      and load them in both `//build/config/BUILD.bazel` and `cc_features.bzl`,
      guarded by `LINT.IfChange` / `LINT.ThenChange` comments.

---

## 6. Step 5: Verification Workflow

1.  **Format modified GN and Bazel files**:

    ```bash
    fx format-code --files=path/to/BUILD.bazel,path/to/BUILD.gn
    ```

2.  **Run Command-Line Equivalence Verification**:

    ```bash
    fx build //build/beads:tests
    ```

    - Regenerates `@fuchsia_build_info//default_build_flags/BUILD.bazel` and
      verifies that normalized compiler/linker command lines match between GN
      and Bazel across `host`, `fuchsia_platform`, and `fuchsia_sdk` (OOT).
