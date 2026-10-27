# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/commit_msg.star."""

load("//scripts/shac/commit_msg.star", "_commit_msg_footers")

def test_no_commits():
    res = testing.run(_commit_msg_footers, commits = [])
    asserts.eq(res.findings, ())

def test_valid_commit_msg():
    msg = """\
Subject line

Body paragraph explaining why this change was made.

TAG=agy
CONV=71756d6c-7607-44cb-b358-a7090e17a8bd

Depends-on: 123
Bug: 456
Test: fx test foo_test
Change-Id: I0123456789abcdef0123456789abcdef01234567
"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, ())

def test_misplaced_footers_earlier_paragraph():
    msg = """\
Subject line

Depends-On: 123
Multiply: host_x64/foo_test
Build-Only: true

Some explanatory paragraph.

Cq-Include-Trybots: //src/foo:bar

Change-Id: I0123456789abcdef0123456789abcdef01234567
"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = 'Footer "Depends-On" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 3,
        ),
        testing.finding(
            level = "error",
            message = 'Footer "Multiply" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 4,
        ),
        testing.finding(
            level = "error",
            message = 'Footer "Build-Only" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 5,
        ),
        testing.finding(
            level = "error",
            message = 'Footer "Cq-Include-Trybots" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 9,
        ),
    ))

def test_tag_conv_below_footers():
    msg = """\
Subject line

Body paragraph.

Depends-on: 123
Change-Id: I0123456789abcdef0123456789abcdef01234567

TAG=agy
CONV=71756d6c-7607-44cb-b358-a7090e17a8bd
"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = 'Footer "Depends-on" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 5,
        ),
        testing.finding(
            level = "error",
            message = 'Footer "Change-Id" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 6,
        ),
    ))

def test_quoted_and_untracked_footers_ignored():
    msg = """\
Revert "[bazel] Deduplicate _solib_ symlinks in runfiles"

This reverts commit 1234567890abcdef1234567890abcdef12345678.

Reason for revert: broke something

Bug: 111
Fixed: 222
Test: none

Original change's description:
> Multiply: host_x64/foo_test
> Depends-on: 123
> Change-Id: Iold0123456789abcdef0123456789abcdef012345
# Comments should also be ignored:
# Depends-on: 999

Run-All-Tests: true
Change-Id: Inew0123456789abcdef0123456789abcdef012345
"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, ())

def test_trailing_blank_lines():
    msg = """\
Subject line

Body paragraph.

Depends-on: 123
Change-Id: I0123456789abcdef0123456789abcdef01234567


"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, ())

def test_single_paragraph_commit_msg():
    """Asserts that footers in a single-paragraph message without blank lines are flagged."""
    msg = """\
Subject line
Depends-on: 123
Change-Id: I0123456789abcdef0123456789abcdef01234567"""

    commit = testing.commit(hash = "0" * 40, message = msg)
    res = testing.run(_commit_msg_footers, commits = [commit])
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = 'Footer "Depends-on" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 2,
        ),
        testing.finding(
            level = "error",
            message = 'Footer "Change-Id" must be in the last paragraph of the commit message to take effect.',
            commit_hash = "0" * 40,
            line = 3,
        ),
    ))

def test_submitted_and_roller_commits_ignored():
    """Asserts that already-submitted commits with Reviewed-on and roller commits are skipped."""

    # Submitted commit where Gerrit appended Reviewed-on in a separate paragraph
    # (e.g. author had a trailing blank line).
    msg1 = """\
Subject line

Depends-on: 123

Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1905419
"""

    # Culprit revert commit where Run-All-Tests was in an earlier paragraph and
    # Gerrit appended Reviewed-on in the final paragraph with No-Try.
    msg2 = """\
Revert "foo"

Run-All-Tests: true

No-Try: true
Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1905420
"""

    # Roller commit where Multiply was placed above the separator/roller metadata paragraph.
    msg3 = """\
[roll] Roll third_party/foo

Multiply: host_x64/foo_test

----------------
Roller-URL: https://ci.chromium.org/b/12345
"""

    commits = [
        testing.commit(hash = "0" * 40, message = msg1),
        testing.commit(hash = "1" * 40, message = msg2),
        testing.commit(hash = "2" * 40, message = msg3),
    ]
    res = testing.run(_commit_msg_footers, commits = commits)
    asserts.eq(res.findings, ())
