# Bazel style guide and best practices

[TOC]

## Overview

This guide defines the formatting, coding conventions, and best practices for
Bazel `BUILD.bazel` and `.bzl` files within the Fuchsia platform codebase.

Note: Although it is within the same codebase, the Fuchsia Bazel SDK (files in
`*/bazel_sdk/*`) may follow different practices.

The Fuchsia project follows the public Bazel
[BUILD Style Guide][bazel-official-build-style]{:.external} and
[.bzl Style Guide][bazel-official-bzl-style]{:.external}, with a few exceptions.
In addition, the Fuchsia project enforces some recommendations and best
practices.

In addition to covering those exceptions, this guide captures advice from other
parts of the Bazel documentation, codifies some best practices, provides
guidance related to paths and symbols specific to the Fuchsia codebase,
highlights equivalents and differences for those familiar with the use of GN in
the Fuchsia codebase, and adds guidance to facilitate consistency and
readability in the Fuchsia codebase.

### Status

This document is under active development. It represents the current set of
common practices and expectations for Bazel files in the Fuchsia platform
codebase. It is not yet comprehensive, and will evolve.

See [Feedback][feedback] for information on how to provide feedback or suggest
additions or other changes.

## Fuchsia-specific style and best practices

The [common style guide and best practices][common-guide] apply to all Bazel
files.

There is additional guidance specific to:

* [`BUILD.bazel` files][build-files-guide]

* [`.bzl` files][bzl-files-guide]

* [Defining rules, macros, and functions][rules-macros-guide]

## Automated tooling {:#automated-tooling}

Format code locally using `fx format-code` before uploading for review.

Over time, mechanically verifiable parts of this guidance will be enforced on
the `static-checks` CQ bots via SHAC checks. This already includes running
`buildifier`.

It is recommended to run these checks locally before uploading for review by
running `fx host-tool shac check`.

Note: Fuchsia Visual Studio Code workspaces do not currently support
`buildifier` (see [issue 522972669][fxbug-522972669]{:.external}).

## Process

This document is owned by the Fuchsia Build team.

### Feedback {:#feedback}

Contact the Fuchsia Build team and/or [file a bug][file-a-bug]{:.external}.

<!-- Reference links -->

[bazel-official-build-style]: https://bazel.build/build/style-guide
[bazel-official-bzl-style]: https://bazel.build/rules/bzl-style
[build-files-guide]: build_files.md
[bzl-files-guide]: bzl_files.md
[common-guide]: common.md
[feedback]: #feedback
[file-a-bug]: https://issues.fuchsia.dev/issues/new?component=1477645
[fxbug-522972669]: https://fxbug.dev/522972669
[rules-macros-guide]: rules_macros.md