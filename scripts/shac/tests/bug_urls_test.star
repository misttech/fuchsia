# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/bug_urls.star."""

load("//scripts/shac/bug_urls.star", "bug_urls")

# Split up so that the bug_urls check doesn't flag this file.
_SHORT = "fx" + "b/"
_HTTP = "http://" + "fxbug.dev/"
_CANONICAL = "https://" + "fxbug.dev/"

def _bug_url_finding(line, col, end_col, bug):
    return testing.finding(
        level = "warning",
        message = "Bug links should use the form %s%s." % (_CANONICAL, bug),
        filepath = "foo.cc",
        line = line,
        col = col,
        end_col = end_col,
        replacements = [_CANONICAL + bug],
    )

def test_bug_urls_short_link():
    res = testing.run(bug_urls, files = {
        "foo.cc": "// TODO(%s123): fix\n" % _SHORT,
    })
    asserts.eq(res.findings, (_bug_url_finding(1, 9, 16, "123"),))
    asserts.eq(res.files["foo.cc"], "// TODO(%s123): fix\n" % _CANONICAL)

def test_bug_urls_http():
    res = testing.run(bug_urls, files = {
        "foo.cc": "// %s456\n" % _HTTP,
    })
    asserts.eq(res.findings, (_bug_url_finding(1, 4, 4 + len(_HTTP) + 3, "456"),))
    asserts.eq(res.files["foo.cc"], "// %s456\n" % _CANONICAL)

def test_bug_urls_multiple_per_line():
    res = testing.run(bug_urls, files = {
        "foo.cc": "%s1 %s2\n" % (_SHORT, _SHORT),
    })
    asserts.eq(res.findings, (
        _bug_url_finding(1, 1, 6, "1"),
        _bug_url_finding(1, 7, 12, "2"),
    ))
    asserts.eq(res.files["foo.cc"], "%s1 %s2\n" % (_CANONICAL, _CANONICAL))

def test_bug_urls_canonical():
    res = testing.run(bug_urls, files = {
        "foo.cc": "// TODO(%s123): fix\n" % _CANONICAL,
    })
    asserts.eq(res.findings, ())

def test_bug_urls_markdown_link_title():
    res = testing.run(bug_urls, files = {
        "foo.cc": "[%s123](%s123)\n" % (_SHORT, _CANONICAL),
    })
    asserts.eq(res.findings, ())

def test_bug_urls_only_affected_lines():
    res = testing.run(bug_urls, files = {
        "foo.cc": testing.file(
            content = "%s1\nok\n" % _SHORT,
            affected_lines = {2: "ok"},
        ),
    })
    asserts.eq(res.findings, ())
