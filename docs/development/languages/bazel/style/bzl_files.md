# .bzl files

[TOC]

## Overview

The Fuchsia project follows the official Bazel
[.bzl Style Guide][bazel-official-bzl-style]{:.external}, with a few exceptions.
The guidance on this page is in addition to the
[common Bazel style guide and best practices][common-guide].

`.bzl` files are most often used for
[Defining rules, macros, and functions][rules-macros-guide], for which there is
additional guidance.

This page is part of the
[Bazel style guide and best practices][style-guide-landing-page].

## Visibility

### Load visibility {:#load-visibility}

Unlike `.gni` files, `.bzl` files support
[load visibility][bazel-load-visibility]{:.external} to restrict from where they
can be loaded. By default, there are no restrictions.

Always specify `visibility` in `.bzl` files
_\[<span style="color:red">SHAC error</span>\]_:

* Use `visibility("private")` for any `.bzl` files not specifically intended for
  use outside the package.

  * Especially when a package contains a mix of public and private `.bzl`
    files, it may make sense to put the latter in a `private/` directory. Bazel
    has mechanisms to restrict loading of files in such directories.

* Otherwise, use `visibility([...])` to specify the packages (directories) that
  may use it.

Load `visibility` uses a different syntax than target `visibility`:

* Just the path is the equivalent of `:__pkg__`.

* The path followed by `/...` is the equivalent of `:__subpackages__`.

For very common targets truly meant to be used across the code base, `"//..."`
may be used. [Do not use public visibility][do-not-use-public-visibility]
(`visibility("public")`) _\[<span style="color:red">SHAC error</span>\]_. In
most cases, though, a set of top-level and/or second-level directories (e.g.,
`"//src/..."` and `"//sdk/lib/..."`) is sufficient.

### Symbol visibility

Within `.bzl` files, prefix symbol names with an underscore to restrict access
to within the `.bzl` file, preventing use by other files even within the same
package (directory).

## Do not create defs.bzl files

Though you may see them, especially in third-party repositories, defining or
exporting multiple symbols that are not certain to be used together, as is
often the case in `defs.bzl` files, is an anti-pattern. See
[Limit the symbols exported by each `.bzl`
file][bazel-official-build-style-limit-symbols]{:.external}. In all cases,
choose a more descriptive file name.

<!-- Reference links -->

[bazel-load-visibility]: https://bazel.build/concepts/visibility#load-visibility
[bazel-official-build-style-limit-symbols]: https://bazel.build/build/style-guide#limit-symbols
[bazel-official-bzl-style]: https://bazel.build/rules/bzl-style
[common-guide]: common.md
[do-not-use-public-visibility]: common.md#do-not-use-public-visibility
[rules-macros-guide]: rules_macros.md
[style-guide-landing-page]: README.md
