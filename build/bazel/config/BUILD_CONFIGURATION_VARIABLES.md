# Propagating Build Configuration Variables from GN to Bazel

This document explains how Fuchsia build configuration variables are represented
in GN and Bazel, why the two build systems handle configuration contexts
differently, and how to propagate configuration variables from GN to the Bazel
build graph.

## Background: Heterogeneous Build & Evaluation Contexts

The Fuchsia build is not homogeneous: a single build invocation must produce
artifacts for many distinct environments, including:

- Host tools (e.g., Linux or macOS binaries running on the build machine)
- Fuchsia user-space device binaries and packages
- Bootloaders and firmware images
- Kernel images (Zircon, Physboot)
- Sanitized or instrumented variants of any of the above (e.g., ASan, HWASan,
  UBSan, coverage, profiling)

Each family of artifacts requires a distinct **evaluation context** for build
configuration information (such as `current_cpu`, `is_debug`, `optimize`, or
`is_profile`).

### GN `toolchain()` vs. Bazel `platforms` and Build Configurations

In **GN**, an evaluation context is called a `toolchain()`. In **Bazel**, target
environments and flag states are modeled through `platforms` and **build
configurations**. While these concepts serve a similar purpose, the two build
systems evaluate and propagate configuration information in fundamentally
different ways:

- **GN evaluates build files per toolchain context**:
  Every `BUILD.gn` and `.gni` file is parsed and evaluated imperatively within
  the context of a specific `toolchain()`. Build configuration values are bound
  directly to GN variables whose values depend on the active `toolchain()`
  context. When a `BUILD.gn` file runs `if (is_profile)` or `if (use_thinlto)`,
  the exact toolchain context is already known.

- **Bazel separates load time from analysis time**:
  - **Load time**: When `.bzl` extensions and `BUILD.bazel` files are evaluated
    to instantiate targets and macros, the target platform and build
    configuration details are **not known yet**. A single load-time target
    definition in a `BUILD.bazel` file may later be analyzed under multiple
    platforms or transitioned configurations.
  - **Analysis time**: After target loading completes, Bazel resolves the
    dependency graph by applying a specific **build configuration** (and target
    platform) to each target definition. Only at analysis time—either through
    `select()` expressions resolved on rule attributes or inside a rule's
    implementation function (`ctx`)—is the exact platform and flag configuration
    known.

---

## Classification of Build Configuration Variables

In GN build files (`BUILD.gn` and `.gni`), **all** build configuration values
look and behave identically: whether a variable is global or toolchain-specific,
and whether it is declared in `declare_args()` or computed from an expression,
it is simply read as a standard GN variable. This distinction does not appear in
GN build files at all.

In Bazel, however, the separation between load time and analysis time requires
classifying GN configuration variables across **two axes**:

### Axis 1: Global vs. Toolchain-Specific Variables

- **Global variables**:
  Invariant for a given `args.gn` definition. Their value is determined once for
  the entire build and never changes based on the GN `toolchain()` context.
  - *Examples in Fuchsia*:
    - `target_cpu`, `target_os`, `host_cpu`, `host_os` (global target and host
      architectures)
    - `sdk_id` (`//sdk/config.gni`)
    - `idk_buildable_cpus`, `warn_on_sdk_changes` (`//build/sdk/config.gni`)
    - `build_info_version`, `build_info_product` (`//build/info/info.gni`)
    - `update_goldens` (`//build/testing/config.gni`)

- **Toolchain-specific variables**:
  Have a value that depends on the current GN `toolchain()` context (for
  instance, variables overridden in `toolchain_args`), or on the target Bazel
  platform and/or Bazel build configuration.
  - *Examples in Fuchsia*:
    - `current_cpu`, `current_os` (set per `toolchain()` to target `x64`,
      `arm64`, `riscv64`, `fuchsia`, `linux`, `unknown`/EFI, etc.)
    - `is_host`, `is_fuchsia`, `is_kernel`, `is_efi_toolchain`
      (`//build/config/BUILDCONFIG.gn`)
    - `is_debug`, `optimize`, `is_profile` (`//build/config/compiler.gni`,
      frequently overridden in variant, kernel, or bootloader `toolchain_args`)
    - `zircon_asserts` (`//build/config/fuchsia/zircon_asserts.gni`)
    - `toolchain_variant.tags` and `toolchain_variant.instrumented`
      (`//build/config/BUILDCONFIG.gn` and `//build/toolchain/variant_tags.gni`,
      identifying active build variant properties such as `"lto"`, `"cfi"`,
      `"asan"`, or `"instrumented"`)

### Axis 2: Declared vs. Derived Values

- **Declared values**:
  Defined using GN `declare_args()` blocks. They must provide a default value,
  can be overridden by the user or product/board configuration in `args.gn`, and
  can also be overridden in GN by the `toolchain_args` scope of a given
  `toolchain()` definition.
  - *Examples in Fuchsia*:
    - Declared with a constant default: `is_debug = true`, `is_profile = false`,
      `sdk_id = ""`, `update_goldens = false`.
    - Declared with a **derived default expression** (where one `declare_args()`
      block references a variable declared or computed earlier):
      - `zircon_asserts = is_debug` (`//build/config/fuchsia/zircon_asserts.gni`)
      - `build_info_board = board_name` (`//build/info/info.gni`)
      - `optimize` defaulting to `"debug"` when `is_debug` is true and
        `"size_thinlto"` otherwise (`//build/config/compiler.gni`)

- **Derived values**:
  Computed from an expression that references declared values or other derived
  values outside of `declare_args()` (cannot be directly overridden in
  `args.gn`).
  - *Examples in Fuchsia*:
    - `is_host = current_os == "linux" || current_os == "mac"`
    - `use_thinlto = (optimize == "size_thinlto" || optimize == "speed_thinlto") && !is_host && !is_profile && (current_cpu == "x64" || current_cpu == "arm64")`

> [!NOTE]
> **Chained `declare_args()` Defaults in GN vs. Bazel**:
> In GN, a variable inside `declare_args()` can use a derived expression (such
> as another variable) as its default value, creating a dependency chain between
> declared variables. In Bazel, `build_setting()` targets (`bool_flag`,
> `string_flag`, etc.) only allow static load-time constants for
> `build_setting_default`.
>
> - **When values are resolved by GN (`args.bzl` and `bazel_args.gni`)**: This
>   dependency chain is already resolved by GN before invoking Bazel, so passing
>   the resolved value via `gn_build_variables_for_bazel` or `bazel_args.gni`
>   works out of the box.
> - **When Bazel transitions modify an upstream variable**: If an internal Bazel
>   configuration transition changes an upstream setting (e.g., `is_debug`), any
>   downstream `build_setting()` whose default depended on `is_debug` in GN (e.g.,
>   `zircon_asserts`) will *not* automatically recompute in Bazel unless the
>   transition also updates `zircon_asserts`, or the variable is backed by a
>   `DerivedConfigValueInfo` rule that falls back to `is_debug` when no explicit
>   override was set.

### Summary of the Four Categories

Combining these two axes yields four categories of build configuration
variables, each mapped differently to Bazel. Details and step-by-step
instructions for each mapping method are provided in the
[Detailed Mapping Guide](#detailed-mapping-guide) below.

| Scope \ Origin | Declared (`declare_args()`) | Derived (Computed Expression) |
| :--- | :--- | :--- |
| **Global**<br>*(Invariant across toolchains)* | **1. Global Declared Values**<br>Exported via `//build/bazel:gn_build_variables_for_bazel` into `@fuchsia_build_info//:args.bzl`.<br>*(Available at Bazel Load Time)* | **2. Global Derived Values**<br>Recomputed in a `.bzl` file using inputs from `@fuchsia_build_info//:args.bzl` (kept in sync with `.gni` via `LINT`).<br>*(Available at Bazel Load Time)* |
| **Toolchain-Specific**<br>*(Varies per toolchain/platform)* | **3. Toolchain-Specific Declared Values**<br>`build_setting()` in `//build/bazel/config/variables` + CLI flags in `bazel_args.gni` + `config_setting()` in `//build/bazel/config/select/<name>`.<br>*(Available at Bazel Analysis Time)* | **4. Toolchain-Specific Derived Values**<br>Custom Starlark `rule()` in `//build/bazel/config/derived/<name>` reading `BuildSettingInfo` and returning `DerivedConfigValueInfo`.<br>*(Available at Bazel Analysis Time)* |

---

## Detailed Mapping Guide

### 1. Global Declared Values

Because global declared values are invariant across all toolchains and platforms
for a given `args.gn`, they can be converted directly into Starlark constants
inside `@fuchsia_build_info//:args.bzl`.

#### How to expose a global declared value

Add a new entry for the variable in the `//build/bazel:gn_build_variables_for_bazel`
target defined in `//build/bazel/BUILD.gn`, guarded by `LINT.IfChange` and
`LINT.ThenChange` comments linking to the `.gni` file where the variable is
declared:

```gn
# In //build/bazel/BUILD.gn (inside generated_file("gn_build_variables_for_bazel"))
# LINT.IfChange
declaration = "//path/to/my_args.gni"
import(declaration)
contents += [
  {
    name = "my_global_variable"
    value = my_global_variable
    type = "bool"
    location = declaration
  },
]
# LINT.ThenChange(//path/to/my_args.gni)
```

#### How to use in Bazel

The constant can be imported and used directly at **load time** in any
`BUILD.bazel` or `.bzl` file:

```starlark
load("@fuchsia_build_info//:args.bzl", "my_global_variable")
```

> [!IMPORTANT]
> **Supported Types and Limitations**:
> Only a limited set of primitive types is supported by
> `gn_build_variables_for_bazel`:
> - `bool`: Mapped to Starlark `True` or `False`.
> - `string`: Mapped to a Starlark `string`.
> - `string_or_false`: Either `false` (mapped to `""`) or a `string`.
> - `array_of_strings`: Mapped to a Starlark list of strings (mixed-type arrays
>   are not permitted).
> - `path`: A GN path beginning with `//` (mapped with `//` stripped) or an
>   absolute path `/`.
>
> **GN scopes are not supported**, and **nested lists (lists of lists) or lists
> of scopes are not supported either**.

---

### 2. Global Derived Values

Global derived values are also invariant across toolchains, so their values can
be computed at **load time** in Starlark.

#### How to expose and maintain a global derived value

1. Replicate the GN computation in a `.bzl` file that loads its input constants
   from `@fuchsia_build_info//:args.bzl` (or from other `.bzl` files that define
   prerequisite global derived values).
2. Keep the `.bzl` calculation and the `.gni` calculation synchronized using
   `LINT.IfChange` and `LINT.ThenChange` annotations in both files.

```gn
# //path/to/config.gni
# LINT.IfChange(derived_feature_flag)
enable_special_feature = my_global_variable && target_cpu == "arm64"
# LINT.ThenChange(//path/to/config.bzl:derived_feature_flag)
```

```starlark
# //path/to/config.bzl
load("@fuchsia_build_info//:args.bzl", "my_global_variable", "target_cpu")

# LINT.IfChange(derived_feature_flag)
enable_special_feature = my_global_variable and target_cpu == "arm64"
# LINT.ThenChange(//path/to/config.gni:derived_feature_flag)
```

For example, `//build/bazel/config/derived/compilation_modes.bzl` derives
`is_debug`, `is_balanced`, `is_release`, and `is_sanitizer` from the global
`compilation_mode` arg.

---

### 3. Toolchain-Specific Declared Values

Toolchain-specific declared values can differ between GN toolchains or Bazel
platforms/configurations. Consequently, their values are unknown at Bazel load
time and must be modeled as Bazel **build settings** evaluated at analysis time.

#### How to expose a toolchain-specific declared value

1. **Define a `build_setting()` target**:
   Add a build setting flag (such as `bool_flag` or `string_flag` from
   `@bazel_skylib//rules:common_settings.bzl`) in
   `//build/bazel/config/variables/BUILD.bazel`.
2. **Propagate the value from GN**:
   Modify `//build/bazel/config/bazel_args.gni` to append
   `--//build/bazel/config/variables:<varname>=<value>` to the build arguments
   list (`_build_args`). This guarantees that each platform/toolchain configuration
   receives the exact `build_setting()` value computed by GN.
   *(Note: If the value of a toolchain-specific declared variable is computed
   from a complex expression in GN, keep that expression in the GN definition for
   now and pass only the resolved value to Bazel.)*
3. **Define `config_setting()` targets for `select()`**:
   Create matching `config_setting()` targets under
   `//build/bazel/config/select/<varname>:<value>` so `BUILD.bazel` files can
   use them as `select()` keys for conditional attributes.

#### Concrete Example: `is_profile`

- **Variable definition** in `//build/bazel/config/variables/BUILD.bazel`:
  ```starlark
  load("@bazel_skylib//rules:common_settings.bzl", "bool_flag")

  bool_flag(
      name = "is_profile",
      build_setting_default = False,
      visibility = ["//visibility:public"],
  )
  ```
- **GN flag propagation** in `//build/bazel/config/bazel_args.gni`:
  ```gn
  if (is_profile) {
    _build_args += [ "--//build/bazel/config/variables:is_profile=True" ]
  }
  ```
- **Select conditions** in `//build/bazel/config/select/is_profile/BUILD.bazel`:
  ```starlark
  config_setting(
      name = "true",
      flag_values = {
          "//build/bazel/config/variables:is_profile": "True",
      },
  )

  config_setting(
      name = "false",
      flag_values = {
          "//build/bazel/config/variables:is_profile": "False",
      },
  )

  alias(
      name = "is_profile",
      actual = ":true",
  )
  ```

> [!NOTE]
> **Why `variables:<varname>` and `select/<varname>:<value>` are separated**:
> - **No naming ambiguity or collisions**: Naming predicates in a flat package as
>   `<varname>_<value>` creates ambiguity when `<varname>` or `<value>` already
>   contains underscores, while placing both `build_setting()` and
>   `config_setting()` in a single `variables/<varname>/BUILD.bazel` package
>   risks target-name collisions between the `build_setting()` and valid string
>   values.
> - **Visual clarity at use sites**: `//build/bazel/config/variables:<varname>`
>   always identifies a `build_setting()` (used in `bazel_args.gni` CLI flags
>   and rule `attr.label()` dependencies), whereas
>   `//build/bazel/config/select/<varname>:*` always identifies a
>   `config_setting()` predicate (used in `select()` blocks).
> - **Boolean shorthand in `select()`**: Having a dedicated package
>   `//build/bazel/config/select/<varname>` allows defining
>   `alias(name = "<varname>", actual = ":true")`, enabling clean boolean
>   shorthand (`"//build/bazel/config/select/is_profile"`) in `select()`
>   dictionaries.
> - **Minimal file count**: All `build_setting()` flags passed on the command
>   line are loaded from a single `//build/bazel/config/variables/BUILD.bazel`
>   package, while `//build/bazel/config/select/<varname>/BUILD.bazel` files only
>   need to be created for variables that are actually used in `select()`
>   statements.

#### Reading the value in custom Starlark rules

For more complex use cases, a custom rule implementation function can access a
toolchain-specific declared variable at **analysis time** by declaring a label
attribute pointing to `//build/bazel/config/variables:<varname>` and indexing
`BuildSettingInfo`:

```starlark
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

def _my_rule_impl(ctx):
    is_profile = ctx.attr._is_profile[BuildSettingInfo].value
    ...
```

#### Special Case: GN Toolchain Variant Tags (`//build/bazel/config/toolchain_tags/`)

Many GN `config()` definitions check whether a specific tag string is present in
`toolchain_variant.tags` (defined in `//build/toolchain/variant_tags.gni`), or
check `toolchain_variant.instrumented` (which is true whenever `"instrumented"`
is present in `toolchain_variant.tags`).

Because there are many such tags and their Bazel definitions follow a uniform
pattern, they are managed in a dedicated `//build/bazel/config/toolchain_tags/`
hierarchy generated by `//build/bazel/config/toolchain_tags/update.py` (see
`//build/bazel/config/toolchain_tags/README.md` for details):

- **`bool_flag()` targets**: Defined in
  `//build/bazel/config/toolchain_tags/BUILD.bazel` as
  `//build/bazel/config/toolchain_tags:<tag>` (rather than under
  `//build/bazel/config/variables`, which is reserved for direct GN variables).
- **GN flag propagation**: `//build/bazel/config/toolchain_tags/toolchain_tags.gni`
  is imported by `//build/bazel/config/bazel_args.gni` to pass
  `--//build/bazel/config/toolchain_tags:<tag>={True,False}` based on
  `toolchain_variant.tags`.
- **`config_setting()` predicates for `select()`**: Defined in
  `//build/bazel/config/toolchain_tags/<tag>/BUILD.bazel` as `:true`, `:false`,
  and `:<tag>` (alias for `:true`).

For example, a GN check for `toolchain_variant.instrumented` or
`toolchain_variant.tags` containing `"lto"` maps to
`//build/bazel/config/toolchain_tags/instrumented:true` and
`//build/bazel/config/toolchain_tags/lto:true` in `select()` statements. To
expose additional tags from `//build/toolchain/variant_tags.gni`, update
`DEFAULT_TAGS_LIST` in `//build/bazel/config/toolchain_tags/update.py` and
re-run the script.

---

### 4. Toolchain-Specific Derived Values

Do **not** create `build_setting()` targets or CLI flags in `bazel_args.gni` for
toolchain-specific derived values. Passing pre-computed derived flags from GN
prevents Bazel transitions (such as transitioning to a host, bootloader, or
variant platform inside the Bazel graph) from accurately recomputing the derived
value when underlying toolchain variables change.

Instead, create a **custom Starlark rule** that depends on the input
`build_setting()` targets, computes the derived value at analysis time, and
returns it via the `DerivedConfigValueInfo` provider (defined in
`//build/bazel/config/providers.bzl`). Each derived variable should be placed in
its own package under `//build/bazel/config/derived/<varname>/BUILD.bazel` with
its rule implementation in `//build/bazel/config/derived/<varname>/defs.bzl`.

#### Example: Translating `use_thinlto`

A GN toolchain-specific derived variable such as:

```gn
# GN: Evaluated imperatively per toolchain
use_thinlto = (optimize == "size_thinlto" || optimize == "speed_thinlto") &&
              !is_host &&
              !is_profile &&
              (current_cpu == "x64" || current_cpu == "arm64")
```

is translated into a custom Starlark rule in
`//build/bazel/config/derived/use_thinlto/defs.bzl`:

```starlark
# //build/bazel/config/derived/use_thinlto/defs.bzl
load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")
load("//build/bazel/config:providers.bzl", "DerivedConfigValueInfo")

def _use_thinlto_impl(ctx):
    # 1. Read input build_setting values for the current target configuration
    optimize = ctx.attr._optimize[BuildSettingInfo].value
    is_profile = ctx.attr._is_profile[BuildSettingInfo].value
    current_cpu = ctx.attr._current_cpu[BuildSettingInfo].value
    is_host = ctx.attr._is_host[BuildSettingInfo].value

    # 2. Run arbitrary Starlark logic (multi-variable branches, string operations, etc.)
    is_supported_cpu = current_cpu in ("x64", "arm64")
    is_lto_opt = optimize in ("size_thinlto", "speed_thinlto")

    computed_value = is_lto_opt and is_supported_cpu and (not is_host) and (not is_profile)

    # 3. Return the provider
    return [
        DerivedConfigValueInfo(
            name = ctx.attr.name,
            value = computed_value,
        ),
    ]

use_thinlto_setting = rule(
    implementation = _use_thinlto_impl,
    attrs = {
        "_optimize": attr.label(default = "//build/bazel/config/variables:optimize"),
        "_is_profile": attr.label(default = "//build/bazel/config/variables:is_profile"),
        "_current_cpu": attr.label(default = "//build/bazel/config/variables:current_cpu"),
        "_is_host": attr.label(default = "//build/bazel/config/variables:is_host"),
    },
)
```

And instantiated in `//build/bazel/config/derived/use_thinlto/BUILD.bazel`:

```starlark
# //build/bazel/config/derived/use_thinlto/BUILD.bazel
load(":defs.bzl", "use_thinlto_setting")

use_thinlto_setting(
    name = "use_thinlto",
    visibility = ["//visibility:public"],
)
```

Downstream custom rules can then depend on `//build/bazel/config/derived/use_thinlto`
via a label attribute and read `ctx.attr._use_thinlto[DerivedConfigValueInfo].value`
during analysis.

> [!WARNING]
> **Limitation with `select()` Statements**:
> Conditional dependencies or attributes in `BUILD.bazel` files using `select()`
> statements **cannot** depend on `DerivedConfigValueInfo` targets. In Bazel,
> `select()` keys only accept predicate targets such as `config_setting()` or
> `constraint_value()` (or `alias` targets pointing to them). In turn,
> `config_setting()` can only match against `build_setting()` labels (via
> `flag_values`), built-in Bazel flags (via `values`), or platform constraints
> (via `constraint_values`)—it **cannot** inspect custom providers like
> `DerivedConfigValueInfo` returned by ordinary Starlark rules.
>
> Consequently, `DerivedConfigValueInfo` targets are intended to be consumed by
> custom rule implementations at analysis time (via `ctx.attr`), rather than
> inside `select()` blocks in `BUILD.bazel` files.
>
> **What to do if a toolchain-specific derived value *must* be used in `select()`**:
> - **For boolean combinations (`AND` / `OR`) of existing `config_setting()`
>   targets**: You can compose existing `//build/bazel/config/select/...`
>   predicates using `selects.config_setting_group(match_all = [...], match_any = [...])`
>   from `@bazel_skylib//lib:selects.bzl`. Because `config_setting_group`
>   expands to native `config_setting()` and `alias()` targets, it works directly
>   in `select()` and respects in-Bazel transitions.
> - **For all other cases**: Because Bazel does not support `build_setting()`
>   targets with dynamically computed defaults, the derived value must instead be
>   implemented as a toolchain-specific `build_setting()` (Category 3) whose
>   value is explicitly set in `//build/bazel/config/bazel_args.gni` based on the
>   GN evaluation of the derived variable.
