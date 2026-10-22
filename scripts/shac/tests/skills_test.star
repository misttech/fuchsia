# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/skills.star."""

load("//scripts/shac/skills.star", "_skill_format")

def test_skill_format_no_skills():
    res = testing.run(_skill_format, files = {
        "README.md": "",
        "src/main.cc": "",
        "XYZSKILL.md": "",
        "src/subsystem/XYZSKILL.md": "",
        "docs/index.md": "",
    })
    asserts.eq(res.findings, ())

def test_skill_format_multiple_subdirectories():
    findings = [
        {
            "filepath": "src/connectivity/wlan/SKILL.md",
            "level": "warning",
            "message": "Warning: Markdown body contains lines exceeding 80 characters.",
            "replacements": ["---\nname: wlan\n---\n\nFormatted.\n"],
        },
        {
            "filepath": "vendor/google/ui/SKILL.md",
            "level": "warning",
            "message": "Warning: Markdown body contains missing empty line before list.",
            "replacements": ["---\nname: ui\n---\n\n* Item 1\n"],
        },
    ]

    res = testing.run(
        _skill_format,
        files = {
            "src/connectivity/wlan/SKILL.md": "unformatted wlan skill\n",
            "vendor/google/ui/SKILL.md": "unformatted ui skill\n",
            "README.md": "",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--suggest-fix-in-json",
                    testing.root + "/src/connectivity/wlan/SKILL.md",
                    testing.root + "/vendor/google/ui/SKILL.md",
                ],
                stdout = json.encode(findings),
            ),
        ],
    )

    asserts.eq(len(res.findings), 2)
    asserts.eq(res.findings[0], testing.finding(
        level = "warning",
        filepath = "src/connectivity/wlan/SKILL.md",
        message = "Warning: Markdown body contains lines exceeding 80 characters.",
        replacements = ["---\nname: wlan\n---\n\nFormatted.\n"],
    ))
    asserts.eq(res.findings[1], testing.finding(
        level = "warning",
        filepath = "vendor/google/ui/SKILL.md",
        message = "Warning: Markdown body contains missing empty line before list.",
        replacements = ["---\nname: ui\n---\n\n* Item 1\n"],
    ))

def test_skill_format_linter_failure():
    res = testing.run(
        _skill_format,
        files = {
            "my_feature/skills/SKILL.md": "broken skill\n",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--suggest-fix-in-json",
                    testing.root + "/my_feature/skills/SKILL.md",
                ],
                retcode = 1,
                stdout = "Traceback (most recent call last):\nFatal error\n",
                stderr = "Exception occurred in parser\n",
            ),
        ],
    )

    asserts.eq(len(res.findings), 1)
    asserts.eq(res.findings[0].level, "error")
    asserts.true("Skill linter error" in res.findings[0].message)
