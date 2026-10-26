# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `gn_visibility_pattern_matches()`."""

load("@bazel_skylib//lib:unittest.bzl", "asserts", "unittest")
load(
    "//build/bazel/rules/components:fx_component.bzl",
    "gn_visibility_pattern_matches",
)

# Table of test cases: (pattern, label_string, expected_match)
_CASES = [
    # Wildcard matches any label in any repository.
    ("*", "//foo:bar", True),
    ("*", "//:root", True),
    ("*", "@bazel_skylib//foo:bar", True),

    # Package wildcard matches any target in that package only.
    ("//pkg:*", "//pkg:foo", True),
    ("//pkg:*", "//pkg:bar", True),
    ("//pkg:*", "//other:foo", False),
    ("//pkg:*", "//pkg/sub:foo", False),

    # Exact target label matches only that target.
    ("//pkg:name", "//pkg:name", True),
    ("//pkg:name", "//pkg:other", False),
    ("//pkg:name", "//other:name", False),

    # Recursive pattern matches the package and any subpackage,
    # but rejects prefix boundary and parent packages.
    ("//a/*", "//a:x", True),
    ("//a/*", "//a/b:x", True),
    ("//a/*", "//a/b/c:x", True),
    ("//a/*", "//ab:x", False),
    ("//a/*", "//ab/c:x", False),
    ("//a/*", "//:x", False),

    # Root recursive wildcard matches any package in the repository.
    ("//*", "//:x", True),
    ("//*", "//a/b:x", True),
    ("//*", "//pkg:target", True),

    # Rejects labels from other repositories.
    ("//a:*", "@bazel_skylib//a:x", False),
    ("//a:x", "@bazel_skylib//a:x", False),
    ("//a/*", "@bazel_skylib//a:x", False),
    ("//*", "@bazel_skylib//a:x", False),

    # Relative patterns (not starting with "//" or "*") are rejected.
    (":x", "//a:x", False),
]

def _gn_visibility_test_impl(ctx):
    env = unittest.begin(ctx)
    for pattern, label_str, expected in _CASES:
        actual = gn_visibility_pattern_matches(pattern, Label(label_str))
        asserts.equals(
            env,
            expected,
            actual,
            "gn_visibility_pattern_matches({}, {})".format(repr(pattern), repr(label_str)),
        )

    # Fail analysis directly as well so `bazel build` on `quick_build_only_tests`
    # catches failures without needing `bazel test` (matching `analysis_failure_test`).
    if env.failures:
        fail("\n".join(env.failures))
    return unittest.end(env)

gn_visibility_test = unittest.make(_gn_visibility_test_impl)
