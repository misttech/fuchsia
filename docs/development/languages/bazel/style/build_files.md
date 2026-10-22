# BUILD.bazel files

[TOC]

## Overview

The Fuchsia project follows the official Bazel
[BUILD Style Guide][bazel-official-build-style]{:.external}, with a few
exceptions. The guidance on this page is in addition to the
[common Bazel style guide and best practices][common-guide].

This page is part of the
[Bazel style guide and best practices][style-guide-landing-page].

## Visibility

### Avoid package default_visibility {:#avoid-package-default-visibility}

Prefer specifying visibility for each target that must be accessible outside
the package rather than declaring package `default_visibility`. In general,
`BUILD.bazel` files are easier to understand when the visibility is specified
in the target definition. Also, using `default_visibility` means that private
targets default to some level of public visibility.

`default_visibility` should only be used in the rare cases where every target
within a `BUILD.bazel` file, now and in the future, should have the same
visibility. This means there are no private helper targets defined in the
`BUILD.bazel` file. An example might be when many widgets are defined and they
all need to be visible to the widget loader or widget tests in another package
(directory).

Do not use `default_visibility`
[for `//visibility:public`][bazel-official-build-style-visibility]{:.external}
and other broad visibility such as `//:__subpackages__` or
`//src:__subpackages__` _\[<span style="color:red">SHAC error</span>\]_.

In GN terms, using `default_visibility = [...]` in a `BUILD.bazel` file is the
equivalent of putting `visibility = [...]` at the top of a `BUILD.gn` file. In
both cases, you would then need to be sure to specify private `visibility` for
each internal target.

### Target visibility {:#target-visibility}

#### Overview

In Bazel:

* Visibility is at the package (directory) level. It is not possible to limit
  visibility to a specific target, either in the same package or another.

  * If this is desired, consider refactoring targets into subdirectories, which
    can be referenced separately.

* Targets are package private by default, meaning they are visible to other
  targets in the same package (`BUILD.bazel` file).

  * This is equivalent to `visibility = [ ":__pkg__" ]`.

  * While visibility can be expanded (see below), it is not possible to prevent
    use by other targets in the same package (`BUILD.bazel` file).

    * If this is desired, consider refactoring the other targets into a
      subdirectory.

* [Target `visibility`][bazel-target-visibility]{:.external} must be specified
  in order for them to be accessible outside the package (directory) where they
  are defined.

##### For those familiar with GN

* Bazel targets are private by default whereas in GN they are public by
  default.

  * The default visibility in a `BUILD.bazel` file is the equivalent of putting
    `visibility = [":*"]` at the top of every `BUILD.gn` file.

* `...:__pkg__` is equivalent to `"...:*"`

* `...:__subpackages__` is equivalent to `".../*"`

* In Bazel, targets are always visible to other targets in the same package
  (directory), and this cannot be changed.

  * In GN, this is equivalent to `visibility` always including `":*"`.

  * When `bazel2gn` is used on a `BUILD.bazel` file with targets that depend on
    each other, you may need to explicitly add `":__pkg__",` to `visibility` in
    the `BUILD.bazel` file even though it is not required for Bazel.

    * When adding such entries, add a comment referring to `bazel2gn` so that
      these can be removed when the GN targets are removed.

#### Use appropriate visibility for each target

Each target's `visibility` should be scoped as tightly as possible.

[Do not use public visibility][do-not-use-public-visibility]
(`"//visibility:public"`) _\[<span style="color:red">SHAC error</span>\]_. For
very common targets that are truly intended to be used across the code base,
`"//:__subpackages__"` is the appropriate string.
However, prefer specifying a few top-level directories (e.g.,
`["//sdk/lib:__subpackages__", "//src:__subpackages__"]`).

[Use `__pkg__` and `__subpackages__` as
appropriate.][bazel-official-build-style-visibility]{:.external}

If package or private visibility is desired, omit the `visibility` attribute as
this is the default.

Note: This exception does not apply to legacy macros where
`visibility = ["//visibility:private"]`
[must be specified][specify-visibility-legacy-macros].

### Visibility for non-obvious target types

`config_setting()` and `exports_files()` support visibility like any other
target except that they are public by default. Always specify `visibility` for
these even though Bazel does not require it.

## target_compatible_with {:#target-compatible-with}

### Host targets

Host targets (such as host tools, host tests, and host-only libraries) should
specify `target_compatible_with = HOST_OS_CONSTRAINTS` (loaded from
`//build/bazel/platforms:constraints.bzl`). This restricts compatibility to the
host operating system independent of the CPU architecture, which allows
developers to cross-compile tools and is necessary for host tools in the IDK
and their dependencies.

`HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl` should not be used
except by the Build team in very rare cases.

<!-- Reference links -->

[bazel-official-build-style]: https://bazel.build/build/style-guide
[bazel-official-build-style-visibility]: https://bazel.build/build/style-guide#visibility
[bazel-target-visibility]: https://bazel.build/concepts/visibility#target-visibility
[common-guide]: common.md
[do-not-use-public-visibility]: common.md#do-not-use-public-visibility
[specify-visibility-legacy-macros]: rules_macros.md#specify-visibility-for-all-targets-defined-by-legacy-macros
[style-guide-landing-page]: README.md
