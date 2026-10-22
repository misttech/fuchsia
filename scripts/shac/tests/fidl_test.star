# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/fidl.star."""

load("//scripts/shac/fidl.star", "_fidl_comment_check")

_MSG = "Use /// instead of // for doc comments in FIDL files preceding declarations or members. fidldoc ignores // comments."

_FIDL = """\
// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
library fuchsia.foo;

// Should be a doc comment.
type Foo = struct {
    // TODO(someone): This is fine.
    a uint32;
    /// Doc comment.
    b uint32;
};

// Floating comment.

// Should also be a doc comment,
// spanning multiple lines.
@available(added=1)
type Bar = struct {};
"""

def _finding(path, line):
    return testing.finding(
        level = "warning",
        message = _MSG,
        filepath = path,
        line = line,
    )

def test_fidl_comment_check():
    res = testing.run(_fidl_comment_check, files = {
        "foo.fidl": _FIDL,
    })
    asserts.eq(res.findings, (
        _finding("foo.fidl", 6),
        _finding("foo.fidl", 16),
        _finding("foo.fidl", 17),
    ))

def test_fidl_comment_check_only_affected_lines():
    res = testing.run(_fidl_comment_check, files = {
        "foo.fidl": testing.file(content = _FIDL, affected_lines = {16: "// Should also be a doc comment,"}),
    })
    asserts.eq(res.findings, (_finding("foo.fidl", 16),))

def test_fidl_comment_check_ignored_files():
    res = testing.run(_fidl_comment_check, files = {
        "foo.test.fidl": _FIDL,
        "foo.cc": _FIDL,
    })
    asserts.eq(res.findings, ())
