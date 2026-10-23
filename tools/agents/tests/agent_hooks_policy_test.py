#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for PreToolUse command schemas and grant policies."""

from __future__ import annotations

import io
import json
import unittest
from collections.abc import Sequence
from unittest import mock

from agents.lib.agent_hooks.grant_policy import (
    GrantPolicyConfig,
    matches_grant_rule,
)
from agents.lib.agent_hooks.pre_tool_use import create_default_command_router
from agents.lib.agent_hooks.pre_tool_use import main as pre_tool_use_main
from agents_testing.base import BaseTestCase


class CommandRuleAndSchemasTest(BaseTestCase):
    """Table-driven tests for CommandRule matching and generated CLI schemas."""

    def assertRuleMatches(
        self,
        rule: str,
        matching: Sequence[str],
        non_matching: Sequence[str] = (),
    ) -> None:
        for cmd in matching:
            with self.subTest(rule=rule, cmd=cmd, expect=True):
                self.assertTrue(matches_grant_rule(cmd, rule), cmd)
        for cmd in non_matching:
            with self.subTest(rule=rule, cmd=cmd, expect=False):
                self.assertFalse(matches_grant_rule(cmd, rule), cmd)

    def test_destructive_git_rm_and_mkfs_rules(self) -> None:
        check = self.assertRuleMatches
        check(
            "command(git clean -f)",
            (
                "git clean -f",
                "git clean -dfx",
                "git clean -fX",
                "git clean --force",
                "git clean -d -x -f",
                "git -C /dir clean -fxd",
                "git -C $(pwd) clean -fdx",
                "git --git-dir=/dir clean -fxd",
                "FOO=1 /usr/bin/git -c core.fsmonitor=false clean -fdx",
            ),
            (
                "git clean",
                "git clean -nd",
                "git clean --dry-run",
                'FOO=1 git clean -n "dir -f name"',
                "git clean -n -efoo",
                "git clean -n -e -f",
            ),
        )
        check(
            "command(git reset --hard)",
            ("git reset --hard", "git -C /d reset -q --hard origin/main"),
            ("git reset", "git reset HEAD -- --hard", "git reset --soft HEAD"),
        )
        check(
            "command(git push --force)",
            (
                "git push --force",
                "git push -f",
                "git push origin main --force",
                "git -C /dir push origin HEAD --force",
                "git push origin $(git branch --show-current) --force",
                "git push -uf origin main",
            ),
            ("git push origin main", "git push -ofoo origin main"),
        )
        check(
            "command(git push --force-with-lease)",
            ("git push --force-with-lease", "git push o --force-with-lease=m"),
            ("git push origin main",),
        )
        check(
            "command(rm -rf /)",
            ("rm -rf /", "rm -rf /*", "/bin/rm -fr /", "rm -rf /tmp /"),
            (
                "rm -rf /var",
                "rm -rf /tmp/foo",
                "rm -rf $(pwd)/",
                "rm -rf",
                "rm file.txt",
            ),
        )
        check(
            "command(rm -rf ~)",
            ("rm -rf ~", "rm -rf ~/", "rm -rf ~/*", "rm -rf $HOME"),
            ("rm -rf ~/dir", "rm -rf ~other", "rm -rf ~/fuchsia/out"),
        )
        check(
            "command(mkfs)",
            ("mkfs /dev/sda1", "/sbin/mkfs -t ext4 /dev/s", "mkfs.ext4 /dev/s"),
            ("ls /dev/sda1", "command -v mkfs", "command -V mkfs"),
        )

    def test_wrappers_delegation_and_schemas(self) -> None:
        check = self.assertRuleMatches
        check("command(whoami)", ("FOO=1 env BAR=2 /usr/bin/whoami 2>&1",))
        check("command(echo DENY)", ("echo DENY > /tmp/out",), ("echo OTHER",))
        check("command(python3 s.py)", ("fuchsia-vendored-python s.py --arg",))
        check(
            "command(fx jiri update)",
            (
                "jiri update",
                "fx jiri update",
                ".jiri_root/bin/jiri -v -j 9 update",
            ),
        )
        check(
            "command(fx ffx target flash)",
            (
                "ffx target flash",
                "ffx -t dev target flash",
                "fx -t dev ffx target flash",
                "fx -- ffx -v target flash",
            ),
            ("ffx target list",),
        )
        check(
            "command(git config --global)",
            ("git config --global a b", "git config set --global a b"),
            ("git config a b", "git config get a"),
        )
        check(
            "command(git config -f)",
            ("git config -f /tmp/c a b", "git config set --file /tmp/c a b"),
            ("git config a b",),
        )
        check(
            "command(fx -i -t d build)",
            ("fx -i -t d build",),
            ("fx -i build -t d", "fx build -i -t d"),
        )
        check("command(find -delete)", ("find s -delete",), ("find s -name a",))
        check(
            "command(uname -a)",
            ("uname -snrvmoa", "uname -s -a"),
            ("uname -s -r",),
        )


class GrantPolicyTest(BaseTestCase):
    """Tests for GrantPolicyConfig, sed -i gating, and main entrypoint."""

    def test_sed_inplace_policy(self) -> None:
        read_only_router = create_default_command_router(
            GrantPolicyConfig(deny_grants=("command(rm -rf /)",))
        )
        deny_sed_router = create_default_command_router(
            GrantPolicyConfig(
                deny_grants=("command(rm -rf /)", "command(sed -i)")
            )
        )
        read_write_router = create_default_command_router(
            GrantPolicyConfig(allow_grants=("command(sed -i)",))
        )
        for cmd in ("sed 's/a/b/g' f", "sed -E -e 's/foo -i bar/baz/' f"):
            self.assertEqual(read_only_router.evaluate(cmd).decision, "allow")
        for cmd in ("sed -i 's/a/b/' f", "sed -Ei 's/a/b/' f", "sed -Ei 0 f"):
            res = read_only_router.evaluate(cmd)
            self.assertEqual(res.decision, "force_ask", cmd)
            assert res.reason is not None
            self.assertIn("Prefer the 'edit_file' tool", res.reason)
            self.assertEqual(
                deny_sed_router.evaluate(cmd).decision, "deny", cmd
            )
            self.assertEqual(
                read_write_router.evaluate(cmd).decision, "allow", cmd
            )
        self.assertEqual(
            read_only_router.evaluate("sed -i 's/a/b/' f && rm -rf /").decision,
            "deny",
        )

    def test_main_entrypoint(self) -> None:
        cfg = self.test_dir / "config.json"
        state_file = self.test_dir / "state.json"
        cfg.write_text(
            json.dumps(
                {
                    "userSettings": {
                        "globalPermissionGrants": {
                            "deny": ["command(git clean -f)"]
                        }
                    }
                }
            ),
            encoding="utf-8",
        )
        state_file.write_text(
            json.dumps(
                {
                    "grant_reasons": {
                        "command(git clean -f)": "Use git stash -u instead."
                    }
                }
            ),
            encoding="utf-8",
        )
        call = {"name": "run_command", "args": {"CommandLine": "git clean -f"}}
        stdin = io.StringIO(json.dumps({"toolCall": call}))
        with mock.patch("sys.stdin", stdin):
            self.assertEqual(
                pre_tool_use_main(
                    ["--config", str(cfg), "--state", str(state_file)]
                ),
                0,
            )
        out = json.loads(self.stdout)
        self.assertEqual(out["decision"], "deny")
        self.assertIn("Use git stash -u instead.", out["reason"])


if __name__ == "__main__":
    unittest.main()
