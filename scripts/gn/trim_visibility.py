#!/usr/bin/env fuchsia-vendored-python
# Copyright 2019 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Deprecated: use build/bazel/scripts/trim_visibility.py instead.

This script used to search BUILD.gn files under each visibility entry for the
target's label as text. That misses consumers added by templates, packages
whose BUILD.gn lives in //build/secondary, and Bazel consumers of migrated
targets, so applying its output could break the build. It now forwards all
arguments to build/bazel/scripts/trim_visibility.py, which finds consumers with
`fx gn refs` and `fx bazel query`, and works on GN-only, dual-built and
Bazel-only targets.

Example usage:
$ fx set ...
$ build/bazel/scripts/trim_visibility.py --target="//build/config:Wno-conversion"
"""

import os
import sys

_SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "..",
    "build",
    "bazel",
    "scripts",
    "trim_visibility.py",
)


def main() -> None:
    print(
        "NOTE: scripts/gn/trim_visibility.py is deprecated, running "
        "build/bazel/scripts/trim_visibility.py instead.",
        file=sys.stderr,
    )
    os.execv(sys.executable, [sys.executable, _SCRIPT] + sys.argv[1:])


if __name__ == "__main__":
    main()
