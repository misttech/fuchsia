# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helpers to guard netstack3 core crates against bad crate variants."""

visibility("private")

def assert_no_deps_tags(all_crates, allowed_crates):
    """Returns assert_no_deps= tags for crates not in allowed_crates.

    Args:
      all_crates: Labels of every core crate variant.
      allowed_crates: Labels of the core crate variants a target may use.

    Returns:
      A list of assert_no_deps=<label> tags.
    """
    return [
        "assert_no_deps=" + c
        for c in all_crates
        if c not in allowed_crates
    ]
