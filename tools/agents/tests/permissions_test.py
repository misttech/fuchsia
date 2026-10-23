#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for permissions definitions and expansion logic."""

from __future__ import annotations

import re
import unittest
from collections.abc import Sequence

from agents.lib import permissions
from agents_testing.base import BaseTestCase


class PermissionsTest(BaseTestCase):
    """Tests for permissions module functions."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_root = self.test_dir

    def _single_pattern(self, template: str) -> str:
        grants = permissions.expand_command_variants(template)
        self.assertEqual(len(grants), 1)
        self.assertTrue(grants[0].startswith("command(regex:"))
        return grants[0][14:-1]

    def assertPatternMatches(
        self,
        template: str,
        matching: Sequence[str],
        non_matching: Sequence[str] = (),
    ) -> None:
        pattern = self._single_pattern(template)
        for cmd in matching:
            with self.subTest(template=template, cmd=cmd, expect=True):
                self.assertTrue(re.fullmatch(pattern, cmd), cmd)
        for cmd in non_matching:
            with self.subTest(template=template, cmd=cmd, expect=False):
                self.assertFalse(re.fullmatch(pattern, cmd), cmd)

    def test_expand_empty_or_comment(self) -> None:
        self.assertEqual(permissions.expand_command_variants(""), [])
        self.assertEqual(permissions.expand_command_variants("   "), [])
        self.assertEqual(
            permissions.expand_command_variants("# this is a comment"), []
        )
        # Inline comments should be stripped
        self.assertPatternMatches("fx test  # run unit tests", ("fx test",))

    def test_expand_comment_with_quoted_hash(self) -> None:
        # Hash inside quotes must not be stripped as a comment
        self.assertPatternMatches(
            'git commit -m "fix #1234"  # inline comment',
            ('git commit -m "fix #1234"',),
        )

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
        self.assertPatternMatches(
            "git status",
            (
                "git status",
                "/usr/bin/git status",
                "git -C /some/dir status",
                'git -C "/dir with spaces" status',
                "git --no-pager status",
                'git -c user.name="John Doe" status',
                "FOO=bar git status",
                "git status --short",
            ),
            ("git-lfs status",),
        )

    def test_expand_git_bare(self) -> None:
        self.assertPatternMatches(
            "git pull",
            (
                "git pull",
                "/usr/bin/git pull",
                "git pull origin main",
                "/usr/bin/git pull origin main",
            ),
            ("git-lfs pull",),
        )

    def test_expand_git_config_scoped(self) -> None:
        non_matching = (
            "git config user.email 'a@b.com'",
            "git config --get user.email",
        )
        for template, positive_cases in (
            (
                "git config --global",
                (
                    "git config --global user.email 'a@b.com'",
                    "git config user.email 'a@b.com' --global",
                    "git -C /dir config --global -l",
                ),
            ),
            (
                "git config --system",
                (
                    "git config --system user.email 'a@b.com'",
                    "git -C /dir config --system -l",
                ),
            ),
            (
                "git config --file",
                (
                    "git config --file ~/.gitconfig user.email 'a@b.com'",
                    "git config --file=/etc/gitconfig user.email 'a@b.com'",
                    "git -C /dir config --file ~/.gitconfig -l",
                ),
            ),
            (
                "git config -f",
                ("git config -f ~/.gitconfig user.email 'a@b.com'",),
            ),
        ):
            self.assertPatternMatches(template, positive_cases, non_matching)

    def test_expand_literal_mode(self) -> None:
        self.assertEqual(
            permissions.expand_command_variants("git clean -f", literal=True),
            ["command(git clean -f)"],
        )
        self.assertEqual(
            permissions.expand_command_variants(
                "/usr/bin/git push --force  # inline comment", literal=True
            ),
            ["command(git push --force)"],
        )
        self.assertEqual(
            permissions.expand_command_variants("whoami", literal=True),
            ["command(whoami)"],
        )
        self.assertEqual(
            permissions.expand_command_variants(
                "command(explicit_rule)", literal=True
            ),
            ["command(explicit_rule)"],
        )
        self.assertEqual(
            permissions.expand_command_variants("# comment only", literal=True),
            [],
        )

    def test_expand_git_quoted_arguments(self) -> None:
        self.assertPatternMatches(
            'git commit -m "fix -f bug"',
            ('git commit -m "fix -f bug"',),
        )

    def test_expand_quoted_binary_path(self) -> None:
        grants = permissions.expand_command_variants(
            '"/custom/path to/git" status'
        )
        self.assertTrue(len(grants) >= 1)
        self.assertTrue(any("git" in g and "status" in g for g in grants))

    def test_expand_fx_variants(self) -> None:
        self.assertPatternMatches(
            "fx test",
            (
                "fx test",
                "fx -t target-name test",
                "fx -t target-name test //foo:bar",
                "fx -t [fe80::1%eth0]:8022 test //foo:bar",
                'fx -t="my-device" test',
                "fx --dir out/default test",
                "fx --dir=out/default -t my-dev -i -x test //pkg:test",
                "scripts/fx -t my-device test",
                "./scripts/fx --enable=incremental test",
                "tools/fx test",
                ".jiri_root/bin/fx test",
                "ENV_VAR=1 fx -t my-dev test",
            ),
            ("fx build", "fx testing", "fxx test"),
        )

    def test_expand_fx_empty_args(self) -> None:
        self.assertPatternMatches(
            "fx", ("fx", "fx -x", "scripts/fx -t my-device build")
        )

    def test_expand_ffx_variants(self) -> None:
        self.assertPatternMatches(
            "ffx target list",
            (
                "ffx target list",
                "ffx -t my-device target list",
                "ffx --machine json target list",
                "ffx -c log.level=Debug -t dev --machine json target list",
                "ffx -v --strict target list",
                "fx ffx target list",
                "fx -t my-dev ffx --machine json target list",
                "scripts/fx -t my-dev ffx -t my-dev2 target list",
                "ENV_VAR=1 ffx -t dev target list",
            ),
            ("ffx target show", "ffx emu start"),
        )

    def test_expand_ffx_empty_args(self) -> None:
        self.assertPatternMatches(
            "ffx",
            (
                "ffx",
                "ffx -v",
                "fx ffx target list",
                "scripts/fx -t my-dev ffx target list",
            ),
        )

    def test_expand_jiri_variants(self) -> None:
        self.assertPatternMatches(
            "jiri status",
            (
                "jiri status",
                "jiri -v status",
                "jiri -vv status",
                "jiri -j 50 status",
                "jiri -root /path/to/root -color never status",
                ".jiri_root/bin/jiri -show-progress=false status",
                "ENV_VAR=1 jiri -q status",
                "jiri -time status",
                "jiri --time status",
                "jiri --quiet status",
                "jiri --show-progress status",
                "jiri -time-log-threshold 5s status",
                "jiri -timefile /tmp/timing.txt status",
                "jiri -progress-window 10 status",
            ),
            ("jiri update", "jiri snapshot"),
        )

    def test_expand_jiri_empty_args(self) -> None:
        self.assertPatternMatches(
            "jiri",
            (
                "jiri",
                "jiri -v",
                "jiri -q status",
                ".jiri_root/bin/jiri -j 10 update",
            ),
        )

    def test_tool_spec_custom_tool(self) -> None:
        spec = permissions.ToolSpec(
            binary_prefix=r"(\S+/)?custom",
            global_flags_pattern=r"(\s+--flag)*",
        )
        grants = spec.expand("build --opt")
        self.assertEqual(len(grants), 1)
        pattern = grants[0][14:-1]
        self.assertTrue(re.fullmatch(pattern, "custom --flag build --opt"))
        self.assertTrue(
            re.fullmatch(pattern, "/path/custom build --opt file.txt")
        )

    def test_expand_python_interpreter_variants(self) -> None:
        py_grants = permissions.expand_command_variants("python3 script.py")
        self.assertIn("command(python script.py)", py_grants)
        self.assertIn("command(python3 script.py)", py_grants)
        self.assertIn("command(fuchsia-vendored-python script.py)", py_grants)
        self.assertIn(
            "command(scripts/fuchsia-vendored-python script.py)", py_grants
        )

    def test_expand_system_binary_regex(self) -> None:
        self.assertPatternMatches(
            "cat",
            (
                "cat",
                "cat file.txt",
                "/bin/cat file.txt",
                "/usr/bin/cat file.txt",
                "LC_ALL=C cat file.txt",
            ),
            ("catalog file.txt", "cat_file"),
        )
        self.assertPatternMatches(
            "ls -la",
            (
                "ls -la",
                "ls -la /some/dir",
                "/bin/ls -la",
                "/usr/bin/ls -la",
                "FOO=bar ls -la",
            ),
            ("ls -l", "ls"),
        )

    def test_expand_trusted_help_variants(self) -> None:
        self.assertPatternMatches(
            "ffx help",
            (
                "ffx --help",
                "ffx help target",
                "ffx target --help",
                "ffx component list --help",
                "ffx -t dev target --help",
            ),
            ("ffx target", "ffx target reboot", "ffx target reboot now"),
        )
        self.assertPatternMatches(
            "cipd help",
            ("cipd --help", "cipd help ensure", "cipd ensure --help"),
            ("cipd ensure",),
        )
        self.assertPatternMatches(
            "echo help",
            ("echo help",),
            ("echo --help",),
        )

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

    def test_read_command_reasons(self) -> None:
        list_file = self.mock_root / "reasons.txt"
        list_file.write_text(
            "# General section header ignored\n"
            "# Reason: Explicitly list files with `git add <file>`.\n"
            "git add -A\n"
            "git add .\n"
            "\n"
            "# Reason: Ignored because followed by blank line\n"
            "\n"
            "git status\n",
            encoding="utf-8",
        )
        reasons: dict[str, str] = {}
        grants = permissions.read_command_list_file(
            list_file, literal=True, reasons=reasons
        )
        self.assertEqual(
            grants,
            [
                "command(git add -A)",
                "command(git add .)",
                "command(git status)",
            ],
        )
        self.assertEqual(
            reasons,
            {
                "command(git add -A)": (
                    "Explicitly list files with `git add <file>`."
                )
            },
        )

    def test_read_command_list_file_nonexistent(self) -> None:
        self.assertEqual(
            permissions.read_command_list_file(
                self.mock_root / "nonexistent.txt"
            ),
            [],
        )

    def test_load_profile_grants(self) -> None:
        fuchsia_dir = self.mock_root
        perm_dir = fuchsia_dir / ".agents" / "config" / "permissions"
        perm_dir.mkdir(parents=True, exist_ok=True)
        (perm_dir / "read_only.txt").write_text(
            "git status\nfx status\n", encoding="utf-8"
        )
        (perm_dir / "local_changes.txt").write_text(
            "git commit\n", encoding="utf-8"
        )
        (perm_dir / "external_changes.txt").write_text(
            "git push\n", encoding="utf-8"
        )
        (perm_dir / "never_allow.txt").write_text(
            "git reset --hard\n", encoding="utf-8"
        )
        (perm_dir / "device_ops.txt").write_text("fx ota\n", encoding="utf-8")
        (perm_dir / "cache_destruction.txt").write_text(
            "fx clean\n", encoding="utf-8"
        )
        (perm_dir / "batch_execution.txt").write_text(
            "find\n", encoding="utf-8"
        )

        grants = permissions.load_profile_grants(fuchsia_dir, "read-only")
        self.assertTrue(any("git" in g and "status" in g for g in grants.allow))
        self.assertIn("command(git reset --hard)", grants.deny)
        self.assertFalse(any("git" in g and "commit" in g for g in grants.deny))
        self.assertFalse(
            any("git" in g and "commit" in g for g in grants.allow)
        )
        self.assertTrue(any("git" in g and "push" in g for g in grants.ask))
        self.assertTrue(any("fx" in g and "ota" in g for g in grants.ask))
        self.assertTrue(any("find" in g for g in grants.ask))

        grants_local = permissions.load_profile_grants(
            fuchsia_dir, "local-changes"
        )
        self.assertTrue(
            any("git" in g and "commit" in g for g in grants_local.allow)
        )
        self.assertTrue(
            any("git" in g and "push" in g for g in grants_local.ask)
        )
        self.assertTrue(
            any("git" in g and "reset" in g for g in grants_local.deny)
        )
        self.assertTrue(any("find" in g for g in grants_local.ask))

        grants_ext = permissions.load_profile_grants(
            fuchsia_dir, "external-changes"
        )
        self.assertTrue(
            any("git" in g and "push" in g for g in grants_ext.allow)
        )
        self.assertTrue(any("find" in g for g in grants_ext.ask))


if __name__ == "__main__":
    unittest.main()
