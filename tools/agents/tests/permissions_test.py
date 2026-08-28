#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for permissions definitions and expansion logic."""

from __future__ import annotations

import pathlib
import re
import tempfile
import unittest
from unittest import mock

from agents.lib import permissions


class PermissionsTest(unittest.TestCase):
    """Tests for permissions module functions."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.mock_root = pathlib.Path(self.temp_dir.name)

    def test_expand_empty_or_comment(self) -> None:
        self.assertEqual(permissions.expand_command_variants(""), [])
        self.assertEqual(permissions.expand_command_variants("   "), [])
        self.assertEqual(
            permissions.expand_command_variants("# this is a comment"), []
        )
        # Inline comments should be stripped
        grants = permissions.expand_command_variants(
            "fx test  # run unit tests"
        )
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "fx test"))

    def test_expand_comment_with_quoted_hash(self) -> None:
        # Hash inside quotes must not be stripped as a comment
        grants = permissions.expand_command_variants(
            'git commit -m "fix #1234"  # inline comment'
        )
        self.assertEqual(len(grants), 1)
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, 'git commit -m "fix #1234"'))

    def test_expand_explicit_syntax(self) -> None:
        self.assertEqual(
            permissions.expand_command_variants("command(foo bar)"),
            ["command(foo bar)"],
        )
        self.assertEqual(
            permissions.expand_command_variants("command(regex:foo.*)"),
            ["command(regex:foo.*)"],
        )

    def test_expand_git_subcommand_regex(self) -> None:
        grants = permissions.expand_command_variants("git status")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]

        # Test pattern matching
        self.assertTrue(re.fullmatch(pattern, "git status"))
        self.assertTrue(re.fullmatch(pattern, "/usr/bin/git status"))
        self.assertTrue(re.fullmatch(pattern, "git -C /some/dir status"))
        self.assertTrue(
            re.fullmatch(pattern, 'git -C "/dir with spaces" status')
        )
        self.assertTrue(re.fullmatch(pattern, "git --no-pager status"))
        self.assertTrue(
            re.fullmatch(pattern, 'git -c user.name="John Doe" status')
        )
        self.assertTrue(re.fullmatch(pattern, "FOO=bar git status"))
        self.assertTrue(re.fullmatch(pattern, "git status --short"))

        # Boundary checks
        self.assertFalse(re.fullmatch(pattern, "git-lfs status"))

    def test_expand_git_force_push_matching(self) -> None:
        grants = permissions.expand_command_variants("git push --force")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]

        self.assertTrue(re.fullmatch(pattern, "git push --force"))
        self.assertTrue(re.fullmatch(pattern, "git push origin main --force"))
        self.assertTrue(
            re.fullmatch(pattern, 'git push "my branch with spaces" --force')
        )
        self.assertTrue(
            re.fullmatch(pattern, "git -C /dir push origin HEAD --force")
        )
        self.assertFalse(re.fullmatch(pattern, "git push origin main"))

    def test_expand_git_quoted_arguments(self) -> None:
        grants = permissions.expand_command_variants(
            'git commit -m "fix -f bug"'
        )
        self.assertTrue(len(grants) >= 1)
        patterns = [g[14:-1] for g in grants if g.startswith("command(regex:")]
        self.assertTrue(
            any(re.fullmatch(p, 'git commit -m "fix -f bug"') for p in patterns)
        )

    def test_expand_quoted_binary_path(self) -> None:
        grants = permissions.expand_command_variants(
            '"/custom/path to/git" status'
        )
        self.assertTrue(len(grants) >= 1)
        self.assertTrue(any("git" in g and "status" in g for g in grants))

    def test_expand_fx_variants(self) -> None:
        grants = permissions.expand_command_variants("fx test")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]

        # Positive matching
        self.assertTrue(re.fullmatch(pattern, "fx test"))
        self.assertTrue(re.fullmatch(pattern, "fx -t target-name test"))
        self.assertTrue(
            re.fullmatch(pattern, "fx -t target-name test //foo:bar")
        )
        self.assertTrue(
            re.fullmatch(pattern, "fx -t [fe80::1%eth0]:8022 test //foo:bar")
        )
        self.assertTrue(re.fullmatch(pattern, 'fx -t="my-device" test'))
        self.assertTrue(re.fullmatch(pattern, "fx --dir out/default test"))
        self.assertTrue(
            re.fullmatch(
                pattern,
                "fx --dir=out/default -t my-dev -i -x test //pkg:test",
            )
        )
        self.assertTrue(re.fullmatch(pattern, "scripts/fx -t my-device test"))
        self.assertTrue(
            re.fullmatch(pattern, "./scripts/fx --enable=incremental test")
        )
        self.assertTrue(re.fullmatch(pattern, "tools/fx test"))
        self.assertTrue(re.fullmatch(pattern, ".jiri_root/bin/fx test"))
        self.assertTrue(re.fullmatch(pattern, "ENV_VAR=1 fx -t my-dev test"))

        # Negative matching
        self.assertFalse(re.fullmatch(pattern, "fx build"))
        self.assertFalse(re.fullmatch(pattern, "fx testing"))
        self.assertFalse(re.fullmatch(pattern, "fxx test"))

    def test_expand_fx_empty_args(self) -> None:
        grants = permissions.expand_command_variants("fx")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "fx"))
        self.assertTrue(re.fullmatch(pattern, "fx -x"))
        self.assertTrue(re.fullmatch(pattern, "scripts/fx -t my-device build"))

    def test_expand_ffx_variants(self) -> None:
        grants = permissions.expand_command_variants("ffx target list")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]

        # Positive matching
        self.assertTrue(re.fullmatch(pattern, "ffx target list"))
        self.assertTrue(re.fullmatch(pattern, "ffx -t my-device target list"))
        self.assertTrue(re.fullmatch(pattern, "ffx --machine json target list"))
        self.assertTrue(
            re.fullmatch(
                pattern,
                "ffx -c log.level=Debug -t my-device --machine json target list",
            )
        )
        self.assertTrue(re.fullmatch(pattern, "ffx -v --strict target list"))
        self.assertTrue(re.fullmatch(pattern, "fx ffx target list"))
        self.assertTrue(
            re.fullmatch(pattern, "fx -t my-dev ffx --machine json target list")
        )
        self.assertTrue(
            re.fullmatch(
                pattern, "scripts/fx -t my-dev ffx -t my-dev2 target list"
            )
        )
        self.assertTrue(
            re.fullmatch(pattern, "ENV_VAR=1 ffx -t dev target list")
        )

        # Negative matching
        self.assertFalse(re.fullmatch(pattern, "ffx target show"))
        self.assertFalse(re.fullmatch(pattern, "ffx emu start"))

    def test_expand_ffx_empty_args(self) -> None:
        grants = permissions.expand_command_variants("ffx")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "ffx"))
        self.assertTrue(re.fullmatch(pattern, "ffx -v"))
        self.assertTrue(re.fullmatch(pattern, "fx ffx target list"))
        self.assertTrue(
            re.fullmatch(pattern, "scripts/fx -t my-dev ffx target list")
        )

    def test_expand_jiri_variants(self) -> None:
        grants = permissions.expand_command_variants("jiri status")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]

        # Positive matching
        self.assertTrue(re.fullmatch(pattern, "jiri status"))
        self.assertTrue(re.fullmatch(pattern, "jiri -v status"))
        self.assertTrue(re.fullmatch(pattern, "jiri -vv status"))
        self.assertTrue(re.fullmatch(pattern, "jiri -j 50 status"))
        self.assertTrue(
            re.fullmatch(
                pattern, "jiri -root /path/to/root -color never status"
            )
        )
        self.assertTrue(
            re.fullmatch(
                pattern, ".jiri_root/bin/jiri -show-progress=false status"
            )
        )
        self.assertTrue(re.fullmatch(pattern, "ENV_VAR=1 jiri -q status"))
        self.assertTrue(re.fullmatch(pattern, "jiri -time status"))
        self.assertTrue(re.fullmatch(pattern, "jiri --time status"))
        self.assertTrue(re.fullmatch(pattern, "jiri --quiet status"))
        self.assertTrue(re.fullmatch(pattern, "jiri --show-progress status"))
        self.assertTrue(
            re.fullmatch(pattern, "jiri -time-log-threshold 5s status")
        )
        self.assertTrue(
            re.fullmatch(pattern, "jiri -timefile /tmp/timing.txt status")
        )
        self.assertTrue(
            re.fullmatch(pattern, "jiri -progress-window 10 status")
        )

        # Negative matching
        self.assertFalse(re.fullmatch(pattern, "jiri update"))
        self.assertFalse(re.fullmatch(pattern, "jiri snapshot"))

    def test_expand_jiri_empty_args(self) -> None:
        grants = permissions.expand_command_variants("jiri")
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "jiri"))
        self.assertTrue(re.fullmatch(pattern, "jiri -v"))
        self.assertTrue(re.fullmatch(pattern, "jiri -q status"))
        self.assertTrue(
            re.fullmatch(pattern, ".jiri_root/bin/jiri -j 10 update")
        )

    def test_tool_spec_custom_tool(self) -> None:
        spec = permissions.ToolSpec(
            binary_prefix=r"(\S+/)?custom",
            global_flags_pattern=r"(\s+--flag)*",
            allow_flags_anywhere=True,
        )
        grants = spec.expand("build --opt")
        self.assertEqual(len(grants), 1)
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "custom --flag build --opt"))
        self.assertTrue(
            re.fullmatch(pattern, "/path/custom build file.txt --opt")
        )

    def test_expand_python_interpreter_variants(self) -> None:
        py_grants = permissions.expand_command_variants("python3 script.py")
        self.assertIn("command(python script.py)", py_grants)
        self.assertIn("command(python3 script.py)", py_grants)
        self.assertIn("command(fuchsia-vendored-python script.py)", py_grants)
        self.assertIn(
            "command(scripts/fuchsia-vendored-python script.py)", py_grants
        )

    def test_expand_sed_inplace_regex(self) -> None:
        sed_grants = permissions.expand_command_variants("sed -i 's/foo/bar/g'")
        self.assertIn("command(sed -i 's/foo/bar/g')", sed_grants)
        regex_grants = [g for g in sed_grants if g.startswith("command(regex:")]
        self.assertTrue(len(regex_grants) > 0)
        pattern = regex_grants[0][14:-1]

        self.assertTrue(re.fullmatch(pattern, "sed -i 's/foo/bar/g' file.txt"))
        self.assertTrue(re.fullmatch(pattern, "/usr/bin/sed -i.bak 's/a/b/' f"))
        self.assertTrue(re.fullmatch(pattern, "sed -E -i '' 's/a/b/' f"))
        self.assertTrue(
            re.fullmatch(pattern, "sed --in-place=suffix 's/a/b/' file.txt")
        )

        # Read-only stream commands containing '-i' in expressions must not trigger in-place regex
        readonly_grants = permissions.expand_command_variants(
            "sed 's/foo -i bar/baz/' file.txt"
        )
        readonly_regexes = [
            g for g in readonly_grants if g.startswith("command(regex:")
        ]
        self.assertEqual(len(readonly_regexes), 0)

    def test_expand_system_binary_resolution(self) -> None:
        with mock.patch("shutil.which", return_value="/usr/bin/ls"):
            with mock.patch("pathlib.Path.exists", return_value=True):
                grants = permissions.expand_command_variants("ls -la")
                self.assertIn("command(ls -la)", grants)
                self.assertIn("command(/usr/bin/ls -la)", grants)
                self.assertIn("command(/bin/ls -la)", grants)

    def test_read_command_list_file(self) -> None:
        list_file = self.mock_root / "commands.txt"
        list_file.write_text(
            "# A comment\n"
            "git status\n"
            "\n"
            "command(explicit_rule)\n"
            "fx build\n",
            encoding="utf-8",
        )
        grants = permissions.read_command_list_file(list_file)
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "git" in g and "status" in g
                for g in grants
            )
        )
        self.assertIn("command(explicit_rule)", grants)
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "fx" in g and "build" in g
                for g in grants
            )
        )

    def test_read_command_list_file_nonexistent(self) -> None:
        self.assertEqual(
            permissions.read_command_list_file(
                self.mock_root / "nonexistent.txt"
            ),
            [],
        )


if __name__ == "__main__":
    unittest.main()
