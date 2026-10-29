# Common style and best practices

[TOC]

## Overview

The following style and best practices apply to all Bazel files in Fuchsia.

This page is part of the
[Bazel style guide and best practices][style-guide-landing-page], which
contains additional guidance for specific scenarios.

## Local variables for lists of source files and dependencies

While the official guide
[discourages dependency
variables][bazel-official-build-style-no-dep-vars]{:.external}, Fuchsia permits
them for managing large, shared lists of source files or dependencies. Exercise
by considering whether the lists are substantially similar or merely share a
few common items.

**No subtraction**: Never remove an item from a list variable.

## Do not use glob() {:#do-not-use-glob}

Do NOT use `glob()` _\[<span style="color:red">SHAC error</span>\]_. List
files explicitly instead.

`fx build` runs Bazel from Ninja actions, and Ninja only tracks the files a
`glob()` matched the last time it ran. If you add a file that a `glob()` would
now match, Ninja doesn't notice. Incremental builds and `fx test` runs then keep
using the old file list. When files are listed explicitly, adding a file means
editing a `BUILD.bazel` file, which Ninja does track.

This rule is specific to Fuchsia. The official Bazel style guide allows
non-recursive globs, and they may be allowed again once Bazel no longer runs
inside Ninja.

Existing uses of `glob()` are listed in `build/bazel/shac/glob_allowlist.json`
at the root of each repository that runs the check (for example,
`//build/bazel/shac/glob_allowlist.json` and
`//vendor/google/build/bazel/shac/glob_allowlist.json`). Don't add new entries
without approval from the Build team. Upstream code under `third_party/`
directories is not checked.

### Never use recursive globs {:#never-use-recursive-globs}

Do NOT use recursive globs (`**`)
_\[<span style="color:red">SHAC error</span>\]_ in any Bazel code, whether or
not Ninja is involved. The official Bazel style guide
[forbids recursive globs for source files][bazel-official-build-style-recursive-globs]{:.external}.
They skip subdirectories that contain their own `BUILD.bazel` file, so it's
hard to tell which files they match. They also cache and parallelize worse than
a `BUILD.bazel` file in each directory.

The only exceptions are specific cases for third-party libraries and other
repositories, such as `//vendor/...` repositories, where it isn't practical to
list every path. Outside `third_party/` directories, these still need an
entry in `build/bazel/shac/recursive_glob_allowlist.json`, which is separate from
the `glob()` allowlist.

## Visibility {:#visibility}

The following are general guidelines that apply to both `BUILD.bazel` and
`.bzl` files. More specific details are provided on the page for each.

### Always specify visibility where possible in both BUILD.bazel and .bzl files

The only scenarios where the attribute is supported that it does not need to be
specified is macros that are private to the package (directory).

Note: This exception does not apply to legacy macros where you must
[Specify visibility for all targets defined by legacy
macros][specify-visibility-legacy-macros].

### Do not use public visibility {:#do-not-use-public-visibility}

Do not use public visibility (`"//visibility:public"` or `visibility("public")`)
_\[<span style="color:red">SHAC error</span>\]_ outside
`//build/bazel_sdk/*rules*/...` directories. This level of access is
[only appropriate if the code is used by external
repositories][bazel-official-build-style-visibility]{:.external}, which is not
applicable to non-SDK code in fuchsia.git.

### Do not reference vendor subpaths {:#do-not-reference-vendor-subpaths}

Do not include vendor subpaths or partner names in visibility declarations
_\[<span style="color:red">SHAC error</span>\]_. Only `//vendor:__pkg__`,
`//vendor:__subpackages__`, `//vendor/google:__pkg__`, and
`//vendor/google:__subpackages__` (or `//vendor/...` and `//vendor/google/...`
in `.bzl` load visibility declarations) may be referenced in public Fuchsia
code.

### [Visibility should be scoped as tightly as possible, while still allowing access by tests and reverse dependencies][bazel-official-build-style-visibility]{:.external}

Visibility should be restricted to only those targets that need it and/or
should be allowed to use it. Be conservative yet practical. For example, if a
target is used within five immediate subdirectories of `//src`, consider using
`//src:__subpackages__` to avoid needing to modify the visibility when a new
use is added. However, if, for example, the target should only be used by
drivers, limit it to packages that implement drivers.

### Use package groups for common non-trivial visibility definitions {:#use-package-groups-for-common-non-trivial-visibility-definitions}

If the `visibility` of multiple targets should be restricted to the same set of
labels, consider representing that set with a
[`package_group`][bazel-package-group]{:.external}. `package_group` also
supports negative visibility when used with targets but not with
[Load visibility][load-visibility].

## Do not mix SDK and platform symbols and targets

**Platform targets (i.e., everything that goes in an AIB or in the IDK) must not
use symbols defined in the Fuchsia Bazel SDK, targets provided by it, or targets
built using it.**

The opposite is also true, targets built using the Fuchsia Bazel SDK should not
depend on platform targets or load symbols from platform `.bzl` files.

### Do not use Fuchsia Bazel SDK paths {:#do-not-use-fuchsia-bazel-sdk-paths}

Platform code should never access file paths containing `bazel_sdk` or Fuchsia
Bazel SDK repository paths _\[<span style="color:red">SHAC error</span>\]_ such
as:

* `@fuchsia_sdk//`

* `@internal_sdk//`

* `@rules_fuchsia//fuchsia`

The only such paths that are permitted begin with `@fuchsia_rules_common/`,
though only the Build team should use these directly.

Platform code should also avoid `bazel_sdk/` paths except in the case of
specific build rules that share implementation with the Fuchsia Bazel SDK.

## Labels for targets and .bzl files

### Referencing targets {:#referencing-targets}

When referencing targets (e.g., in `deps`), labels beginning with any of the
following are permitted as long as
[prohibited label patterns][prohibited-label-patterns] are not used:

* `:`

  * Only use relative labels for targets in the same package (`BUILD.bazel`
    file).

* `//`

  * See [Do not use Fuchsia Bazel SDK paths][do-not-use-fuchsia-bazel-sdk-paths]
    for exceptions.

* `@platforms//`

### Loading from .bzl files

Most general purpose macros and rules for the Fuchsia platform can be found
within `//build/bazel/rules/`.

It is safe to `load()` from `.bzl` files whose labels begin with the following
as long as [prohibited label patterns][prohibited-label-patterns] are not used:

* `:`

  * Only use relative labels for files in the same package (directory).

* `//`

  * See [Do not use Fuchsia Bazel SDK paths][do-not-use-fuchsia-bazel-sdk-paths]
    for exceptions.

* `@bazel_skylib//`

The following are also allowed, though only developers on the Build Team are
likely to use them:

* `@fuchsia_build_config//:defs.bzl`

* `@fuchsia_build_info//:args.bzl`

* `@fuchsia_rules_common//`

### Prohibited label patterns {:#prohibited-label-patterns}

Do NOT use _\[<span style="color:red">SHAC error</span>\]_:

* Workspace root package labels (those starting with `//:`)

  * There are very specific and very rare circumstances where this is needed
    (see [issue 560343570][fxbug-560343570]{:.external}), but this should
    generally only be done by the Build team.

* Labels that contain a slash (`/`) in the package _name_, which is the part of
  the label after the colon (`:`).

  * There are rare exceptions for integrating third-party libraries.

## fuchsia_... files and symbols are in the Fuchsia Bazel SDK {:#fuchsia-files-and-symbols-are-in-the-fuchsia-bazel-sdk}

Avoid defining files, macros, and rules with names that begin with `fuchsia_`.
Existing instances of names beginning with `fuchsia_` likely belong to the
Fuchsia Bazel SDK (see
[Do not use Fuchsia Bazel SDK paths][do-not-use-fuchsia-bazel-sdk-paths]), and
avoiding such names helps maintain that separation.

See [Wrapping built-in and common rules, macros, and
functions][wrapping-rules-macros] for one pattern used when needing to
differentiate Fuchsia platform from general Bazel identifiers.

## Use Fuchsia-specific wrappers {:#use-fuchsia-specific-wrappers}

When Fuchsia-specific wrappers exist, use those
_\[<span style="color:red">SHAC error</span>\]_ rather than external
repositories, macros, etc. This helps ensure that Fuchsia build configurations
are applied consistently.

Specifically, there are wrappers for the following languages:

* C/C++: Use `fx_cc_...()` from `//build/bazel/rules/cc/...` (or
  `cc_*_library_zx()` from `//build/bazel/rules:zx_library.bzl` for Zircon
  libraries) rather than `cc_...` from `@rules_cc//`.

* Rust: Use `rustc_...()` from `//build/bazel/rules/rust/...` rather than
  `rust_...` from `@rules_rust//`.

Fuchsia does not have wrappers for the following languages. Load from the
following paths for consistency:

* Go: `@io_bazel_rules_go//go...`

* Python: `@rules_python//python...`

## Strings

### Use double quotation marks for strings _except to avoid escaping_

[By default, use double quotation marks for
strings.][bazel-official-build-style-python-diff]{:.external} However, if
printing a double quotation would be more appropriate and doing so would
involve escaping the double quotation marks (`\"`), use single quotation marks
to avoid the escaping.

<!-- Reference links -->

[bazel-official-build-style-no-dep-vars]: https://bazel.build/build/style-guide#no-dep-vars
[bazel-official-build-style-python-diff]: https://bazel.build/build/style-guide#differences-python-style-guide
[bazel-official-build-style-recursive-globs]: https://bazel.build/build/style-guide#recursive
[bazel-official-build-style-visibility]: https://bazel.build/build/style-guide#visibility
[bazel-package-group]: https://bazel.build/reference/be/functions#package_group
[do-not-use-fuchsia-bazel-sdk-paths]: #do-not-use-fuchsia-bazel-sdk-paths
[fxbug-560343570]: https://fxbug.dev/560343570
[load-visibility]: bzl_files.md#load-visibility
[prohibited-label-patterns]: #prohibited-label-patterns
[specify-visibility-legacy-macros]: rules_macros.md#specify-visibility-for-all-targets-defined-by-legacy-macros
[style-guide-landing-page]: README.md
[wrapping-rules-macros]: rules_macros.md#wrapping-built-in-and-common-rules-macros-and-functions
