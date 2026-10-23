#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for PreToolUse agent hooks lexer, parser, and routers."""

from __future__ import annotations

import unittest
from collections.abc import Sequence

from agents.lib.agent_hooks.engine.parser import CommandSchema, Flag, Subcommand
from agents.lib.agent_hooks.engine.router import (
    CommandRouter,
    HookPayload,
    PolicyResult,
    ToolCallContext,
    ToolName,
    ToolRouter,
)
from agents.lib.agent_hooks.engine.shell_lexer import (
    split_compound_commands,
    strip_env_assignments,
    strip_redirections,
    tokenize_shell,
)


class _SampleSchema(CommandSchema):
    prog = "mycli"
    global_flags = [
        Flag(("-C",)),
        Flag(("-v", "--verbose"), action="store_true"),
    ]
    subcommands = [
        Subcommand(
            "push",
            flags=[
                Flag(("-f", "--force"), action="store_true"),
                Flag(("-l", "--lease"), action="append", nargs="?"),
            ],
        ),
        Subcommand(
            "remote",
            subcommands=[
                Subcommand(
                    "add", flags=[Flag(("-t", "--track"), action="append")]
                )
            ],
        ),
    ]


class _StrictPrefixSchema(CommandSchema):
    prog = "myfx"
    strict_global_prefix = True
    global_flags = [Flag(("-i",), action="store_true")]
    subcommands = [Subcommand("build")]


class EngineTest(unittest.TestCase):
    """Tests for shell_lexer, CommandSchema, CommandMatch, and routers."""

    def test_shell_lexer_and_heredocs(self) -> None:
        split_cases: tuple[tuple[str, list[str]], ...] = (
            (
                "echo foo && sed -i 's/a/b/' f || git status ; ls | head",
                ["echo foo", "sed -i 's/a/b/' f", "git status", "ls", "head"],
            ),
            (
                "fx test 2>&1\necho bg & cat &>/dev/null",
                ["fx test 2>&1", "echo bg", "cat &>/dev/null"],
            ),
            ('git commit -m "a && b; c"', ['git commit -m "a && b; c"']),
            ("echo 'foo\\' && rm -rf /", ["echo 'foo\\'", "rm -rf /"]),
            (
                "cat <<EOF > /tmp/out\nhello\nEOF\nrm -rf /",
                ["cat <<EOF > /tmp/out", "rm -rf /"],
            ),
            (
                "cat <<-EOF\n\tindented\n\tEOF\necho after",
                ["cat <<-EOF", "echo after"],
            ),
            ('echo "<<EOF"\nnext_cmd', ['echo "<<EOF"', "next_cmd"]),
            (
                "(echo a && whoami)\necho ok # 'comment\nuname -a",
                ["echo a", "whoami", "echo ok", "uname -a"],
            ),
            (
                "echo \"hi $(whoami) `uname -a`\" && echo '$(noop)'",
                [
                    'echo "hi $(whoami) `uname -a`"',
                    "echo '$(noop)'",
                    "whoami",
                    "uname -a",
                ],
            ),
            (
                "cat <<EOF\n$(uname -a)\nEOF\ncat <<'EOF'\n$(noop)\nEOF",
                ["cat <<EOF", "uname -a", "cat <<'EOF'"],
            ),
            (
                r"find . -exec echo -empty \;",
                ["find . -exec ''", "echo -empty"],
            ),
            (
                "git -C $(pwd) clean -fdx && echo `uname -a`",
                [
                    "git -C '__sub__' clean -fdx",
                    "echo '__sub__'",
                    "pwd",
                    "uname -a",
                ],
            ),
            (
                'echo "a\\\nb" && echo "c\\\\\n\\" ; sed -i s/x/y/ f"',
                ['echo "ab"', 'echo "c\\\\\n\\" ; sed -i s/x/y/ f"'],
            ),
        )
        for cmd, expected in split_cases:
            with self.subTest(cmd=cmd):
                self.assertEqual(split_compound_commands(cmd), expected)

        self.assertEqual(tokenize_shell("$'whoami'"), ["whoami"])

        env_cases: tuple[tuple[list[str], list[str]], ...] = (
            (
                ["FOO=1", "env", "-i", "BAR=2", "/usr/bin/whoami"],
                ["/usr/bin/whoami"],
            ),
            (
                [
                    "do",
                    "{",
                    "!",
                    "xargs",
                    "-n",
                    "1",
                    "nice",
                    "-n",
                    "10",
                    "w",
                    "}",
                ],
                ["w"],
            ),
            (["env", "-S", "uname -a"], ["uname", "-a"]),
            (["command", "-v", "mkfs"], ["command", "-v", "mkfs"]),
            (["command", "-p", "whoami"], ["whoami"]),
            (["FOO=1"], []),
        )
        for tokens, expected in env_cases:
            with self.subTest(tokens=tokens):
                self.assertEqual(strip_env_assignments(tokens), expected)

        self.assertEqual(
            strip_redirections(
                [
                    "echo>/dev/null",
                    "0<&1",
                    "T",
                    ">",
                    "/tmp/o",
                    "2>&1",
                    "<<EOF",
                    "<<-EOF",
                    "<<<word",
                ]
            ),
            ["echo", "T"],
        )

    def test_schema_parsing_and_matching(self) -> None:
        m = _SampleSchema.parse_tokens(
            ["mycli", "-C", "/dir", "push", "--lease", "origin", "main", "-u"]
        )
        assert m is not None
        self.assertEqual(m.subcommand_path, ("push",))
        self.assertEqual(m.flags, {"C": "/dir", "lease": [""]})
        self.assertEqual(m.positionals, ("origin", "main"))
        self.assertEqual(m.unknown_flags, ("-u",))

        req = _SampleSchema.parse_tokens(["mycli", "push", "--lease"])
        lease_no = _SampleSchema.parse_tokens(["mycli", "push", "--lease=no"])
        bundle = _SampleSchema.parse_tokens(["mycli", "push", "-fl", "0", "m"])
        assert req is not None and lease_no is not None and bundle is not None
        self.assertTrue(m.matches_flags_of(req))
        self.assertFalse(lease_no.matches_flags_of(req))
        self.assertTrue(bundle.matches_flags_of(req))
        self.assertEqual(bundle.positionals, ("0", "m"))

        dash_arg = _SampleSchema.parse_tokens(["mycli", "-C", "-v", "push"])
        assert dash_arg is not None
        self.assertEqual(dash_arg.flags, {"C": "-v"})
        self.assertEqual(dash_arg.subcommand_path, ("push",))

        unk_space = _SampleSchema.parse_tokens(
            ["mycli", "push", "--extra", "v"]
        )
        unk_eq = _SampleSchema.parse_tokens(["mycli", "push", "--extra=v"])
        assert unk_space is not None and unk_eq is not None
        self.assertTrue(unk_space.matches_flags_of(unk_eq))

        v_true = _SampleSchema.parse_tokens(["mycli", "-v=true"])
        v_false = _SampleSchema.parse_tokens(["mycli", "-v=false"])
        assert v_true is not None and v_false is not None
        self.assertTrue("verbose" in v_true.flags)
        self.assertTrue("verbose" not in v_false.flags)

        nested = _SampleSchema.parse_tokens(
            ["mycli", "remote", "add", "-t", "main", "origin", "url"]
        )
        assert nested is not None
        self.assertEqual(nested.subcommand_path, ("remote", "add"))
        self.assertEqual(nested.flags.get("track"), ["main"])

        m_pre = _StrictPrefixSchema.parse_tokens(["myfx", "-i", "build"])
        m_post = _StrictPrefixSchema.parse_tokens(["myfx", "build", "-i"])
        m_only = _StrictPrefixSchema.parse_tokens(["myfx", "-i"])
        assert m_pre is not None and m_post is not None and m_only is not None
        self.assertTrue("i" in m_pre.flags and "i" not in m_post.flags)
        self.assertTrue("i" in m_only.flags and m_only.subcommand_path == ())

    def test_command_and_tool_routers(self) -> None:
        seen_tokens: list[tuple[str, ...]] = []

        def _check(_sub: str, tokens: Sequence[str]) -> PolicyResult:
            seen_tokens.append(tuple(tokens))
            if tokens[0] in ("deny_me", "rm"):
                return PolicyResult.deny("blocked")
            if tokens[0] == "ask_me":
                return PolicyResult.force_ask("confirm")
            return PolicyResult.allow()

        cmd_router = CommandRouter(checks=(_check,))
        self.assertEqual(cmd_router.evaluate("echo ok").decision, "allow")
        self.assertEqual(
            cmd_router.evaluate("ask_me && echo ok").to_dict(),
            {"decision": "force_ask", "reason": "confirm"},
        )
        self.assertEqual(
            cmd_router.evaluate("ask_me && deny_me").decision, "deny"
        )
        self.assertEqual(
            cmd_router.evaluate("echo 'unclosed").decision, "allow"
        )
        self.assertEqual(
            cmd_router.evaluate("deny_me 'unclosed").decision, "deny"
        )

        seen_tokens.clear()
        heredoc_py = (
            "python3 - <<'EOF'\n"
            "# don't fail on apostrophe\n"
            '"""multiline\nstring"""\n'
            "rm -rf /\n"
            "EOF"
        )
        self.assertEqual(cmd_router.evaluate(heredoc_py).decision, "allow")
        self.assertEqual(seen_tokens, [("python3", "-")])

        seen_ctx: list[ToolCallContext] = []

        def _on_run(ctx: ToolCallContext) -> PolicyResult:
            seen_ctx.append(ctx)
            return cmd_router.evaluate(ctx.args.get("CommandLine", ""))

        tool_router = ToolRouter()
        tool_router.register(ToolName.RUN_COMMAND, _on_run)
        raw_payload = {
            "toolCall": {
                "name": "run_command",
                "args": {"CommandLine": "deny_me"},
            },
            "transcriptPath": "/tmp/t.jsonl",
            "workspacePaths": ["/w"],
            "conversationId": "c1",
        }
        parsed_payload = HookPayload.from_payload(raw_payload)
        assert parsed_payload is not None
        self.assertEqual(parsed_payload.transcript_path, "/tmp/t.jsonl")
        self.assertEqual(parsed_payload.workspace_paths, ("/w",))
        self.assertEqual(parsed_payload.conversation_id, "c1")
        self.assertEqual(tool_router.evaluate(parsed_payload).decision, "deny")
        self.assertEqual(len(seen_ctx), 1)
        self.assertEqual(seen_ctx[0].name, "run_command")
        self.assertEqual(
            tool_router.evaluate({"name": "other", "args": {}}).decision,
            "allow",
        )


if __name__ == "__main__":
    unittest.main()
