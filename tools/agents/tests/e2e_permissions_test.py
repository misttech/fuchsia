#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""End-to-end permissions test harness for the agent CLI contract."""

import argparse
import contextlib
import dataclasses
import io
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import unittest
from collections.abc import Iterator
from typing import Any
from unittest import mock

from agents.commands import setup
from agents.lib import githooks, paths, services
from agents.lib.agent_hooks.engine.router import PolicyResult
from agents.lib.agent_hooks.grant_policy import (
    GrantPolicyConfig,
    matches_grant_rule,
)
from agents.lib.agent_hooks.pre_tool_use import create_default_command_router
from agents_testing import ParallelTextTestRunner, parallel_main
from agents_testing.base import BaseTestCase


@dataclasses.dataclass(frozen=True)
class CommandTestCase:
    """A command to test and its expected permission status."""

    command: str
    expected_status: str  # "ALLOW" or "DENY"
    description: str


def _extract_command_events(transcript_path: str) -> list[dict[str, Any]]:
    """Extract run_command tool calls and outcomes from a transcript file."""
    events: list[dict[str, Any]] = []
    pending_calls: list[dict[str, Any]] = []

    with open(transcript_path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                step = json.loads(line)
            except (json.JSONDecodeError, TypeError, ValueError):
                continue

            step_type = step.get("type")

            # 1. Planner response initiating tool calls
            if step_type == "PLANNER_RESPONSE":
                # Pending tool calls without an output step were denied/blocked.
                while pending_calls:
                    call = pending_calls.pop(0)
                    call["success"] = False

                for tc in step.get("tool_calls", ()):
                    if tc.get("name") == "run_command":
                        raw_args = tc.get("args", {})
                        cmd_str = str(raw_args.get("CommandLine", "")).strip()
                        event = {
                            "command": cmd_str,
                            "success": True,
                            "output": "",
                        }
                        events.append(event)
                        pending_calls.append(event)

            # 2. Tool execution results / outputs / permission errors
            elif step_type in (
                "CODE_ACTION",
                "RUN_COMMAND",
                "COMMAND_STATUS",
                "TOOL_RESULT",
                "GENERIC",
                "SYSTEM_MESSAGE",
                "ERROR_MESSAGE",
            ):
                content = str(step.get("content", ""))
                err = step.get("error")
                if err:
                    content += f" {err}"

                if (
                    step_type == "GENERIC"
                    and "Tool is running as a background task" in content
                ):
                    continue

                if pending_calls:
                    last_call = pending_calls.pop(0)
                    last_call["output"] = content
                    lower_content = content.lower()
                    is_error = (
                        step.get("status") == "ERROR"
                        or step_type == "ERROR_MESSAGE"
                        or "permission check failed" in lower_content
                        or "permission denied" in lower_content
                        or "denied by pre-tool hook" in lower_content
                        or "pretooluse gate:" in lower_content
                    )
                    if is_error:
                        last_call["success"] = False

    while pending_calls:
        call = pending_calls.pop(0)
        call["success"] = False

    return events


def _match_event_for_command(
    expected_command: str,
    events: list[dict[str, Any]],
    used_indices: set[int],
) -> dict[str, Any] | None:
    """Find and consume the recorded event matching a tested command."""
    expected_cleaned = expected_command.strip()
    for idx, ev in enumerate(events):
        if (
            idx not in used_indices
            and ev["command"].strip() == expected_cleaned
        ):
            used_indices.add(idx)
            return ev
    return None


def evaluate_test_cases(
    test_cases: list[CommandTestCase],
    tool_events: list[dict[str, Any]],
) -> tuple[bool, int, int, str]:
    """Compare recorded tool events against expected permissions."""
    all_passed = True
    passed_count = 0
    used_indices: set[int] = set()
    lines: list[str] = ["=== Test Results Summary ==="]
    for tc in test_cases:
        event = _match_event_for_command(tc.command, tool_events, used_indices)
        if not event:
            status_str = "NOT ATTEMPTED"
            passed = False
        else:
            actual_status = "ALLOW" if event["success"] else "DENY"
            passed = actual_status == tc.expected_status
            status_str = (
                f"Actual: {actual_status} (Expected: {tc.expected_status})"
            )

        result_mark = "PASS" if passed else "FAIL"
        if not passed:
            all_passed = False
        else:
            passed_count += 1

        lines.append(f"[{result_mark}] {tc.description}")
        lines.append(f"       Command : {tc.command}")
        lines.append(f"       Outcome : {status_str}")
        lines.append("")

    summary = "\n".join(lines)
    return all_passed, passed_count, len(test_cases), summary


class OfflineTranscriptTest(BaseTestCase):
    """Hermetic unit tests for event extraction, evaluation, and runner."""

    def test_extract_command_events_all_cases(self) -> None:
        """Verify extraction of success, denied, pending, and background."""
        transcript_file = self.test_dir / "transcript.jsonl"
        with transcript_file.open("w", encoding="utf-8") as f:
            # 1. Allowed command with SUCCESS
            f.write(
                json.dumps(
                    {
                        "type": "PLANNER_RESPONSE",
                        "tool_calls": [
                            {
                                "name": "run_command",
                                "args": {"CommandLine": "fx help"},
                            }
                        ],
                    }
                )
                + "\n"
            )
            f.write(
                json.dumps(
                    {
                        "type": "RUN_COMMAND",
                        "status": "SUCCESS",
                        "content": "fx usage help...",
                    }
                )
                + "\n"
            )

            # 2. Denied command with ERROR and deny rule message
            f.write(
                json.dumps(
                    {
                        "type": "PLANNER_RESPONSE",
                        "tool_calls": [
                            {
                                "name": "run_command",
                                "args": {"CommandLine": "git push --dry-run"},
                            }
                        ],
                    }
                )
                + "\n"
            )
            f.write(
                json.dumps(
                    {
                        "type": "RUN_COMMAND",
                        "status": "ERROR",
                        "content": (
                            "permission check failed: "
                            "matches user-configured deny rule"
                        ),
                    }
                )
                + "\n"
            )

            # 3. Background task handled properly (GENERIC followed by SUCCESS)
            f.write(
                json.dumps(
                    {
                        "type": "PLANNER_RESPONSE",
                        "tool_calls": [
                            {
                                "name": "run_command",
                                "args": {"CommandLine": "echo background_task"},
                            }
                        ],
                    }
                )
                + "\n"
            )
            f.write(
                json.dumps(
                    {
                        "type": "GENERIC",
                        "status": "DONE",
                        "content": (
                            "Tool is running as a background task "
                            "with task id: task-1"
                        ),
                    }
                )
                + "\n"
            )
            f.write(
                json.dumps(
                    {
                        "type": "RUN_COMMAND",
                        "status": "SUCCESS",
                        "content": "background task completed successfully",
                    }
                )
                + "\n"
            )

            # 4. Pending tool call without output (drained as DENIED at EOF)
            f.write(
                json.dumps(
                    {
                        "type": "PLANNER_RESPONSE",
                        "tool_calls": [
                            {
                                "name": "run_command",
                                "args": {"CommandLine": "rm -rf /"},
                            }
                        ],
                    }
                )
                + "\n"
            )

        events = _extract_command_events(str(transcript_file))
        self.assertEqual(len(events), 4)

        self.assertEqual(events[0]["command"], "fx help")
        self.assertTrue(events[0]["success"])

        self.assertEqual(events[1]["command"], "git push --dry-run")
        self.assertFalse(events[1]["success"])

        self.assertEqual(events[2]["command"], "echo background_task")
        self.assertTrue(events[2]["success"])

        self.assertEqual(events[3]["command"], "rm -rf /")
        self.assertFalse(events[3]["success"])

    def test_evaluate_test_cases(self) -> None:
        """Verify evaluate_test_cases against expected outcomes."""
        events = [
            {
                "command": "fx help",
                "success": True,
                "output": "ok",
            },
            {
                "command": "git push --dry-run",
                "success": False,
                "output": "permission check failed",
            },
        ]
        test_cases = [
            CommandTestCase("fx help", "ALLOW", "fx help allowed"),
            CommandTestCase("git push --dry-run", "DENY", "git push denied"),
        ]
        passed, passed_count, total_count, summary = evaluate_test_cases(
            test_cases, events
        )
        self.assertTrue(passed)
        self.assertEqual(passed_count, 2)
        self.assertEqual(total_count, 2)
        self.assertIn("=== Test Results Summary ===", summary)

        failing_cases = [
            CommandTestCase("fx help", "DENY", "expected deny"),
        ]
        passed, passed_count, total_count, summary = evaluate_test_cases(
            failing_cases, events
        )
        self.assertFalse(passed)
        self.assertEqual(passed_count, 0)
        self.assertEqual(total_count, 1)

        unattempted_cases = [
            CommandTestCase("unattempted_cmd", "ALLOW", "not attempted"),
        ]
        passed, passed_count, total_count, summary = evaluate_test_cases(
            unattempted_cases, events
        )
        self.assertFalse(passed)
        self.assertEqual(passed_count, 0)
        self.assertEqual(total_count, 1)

    def test_parallel_runner_executes_concurrently(self) -> None:
        """Verify ParallelTextTestRunner runs test cases across threads."""
        barrier = threading.Barrier(2, timeout=5.0)
        events: list[str] = []

        class _SampleParallelTest(unittest.TestCase):
            @classmethod
            def setUpClass(cls) -> None:
                events.append("setUpClass")

            @classmethod
            def tearDownClass(cls) -> None:
                events.append("tearDownClass")

            def test_worker_a(self) -> None:
                barrier.wait()

            def test_worker_b(self) -> None:
                barrier.wait()

        suite = unittest.defaultTestLoader.loadTestsFromTestCase(
            _SampleParallelTest
        )
        stream = io.StringIO()
        runner = ParallelTextTestRunner(stream=stream, max_workers=2)
        result = runner.run(suite)
        self.assertTrue(result.wasSuccessful())
        self.assertEqual(result.testsRun, 2)
        self.assertEqual(events, ["setUpClass", "tearDownClass"])


_IO_LOCK = threading.Lock()


def evaluate_command_hooks(
    command_line: str,
    config_path: pathlib.Path | None = None,
) -> PolicyResult:
    """Evaluate a shell command through the PreToolUse command router."""
    grant_policy = GrantPolicyConfig.from_files(config_path=config_path)
    router = create_default_command_router(grant_policy=grant_policy)
    return router.evaluate(command_line)


def _ensure_hermetic_fuchsia_dir(tpath: pathlib.Path) -> pathlib.Path:
    """Create a deterministic hermetic mock Fuchsia directory."""
    fuchsia_dir = tpath / "mock_fuchsia"
    perm_dir = fuchsia_dir / ".agents" / "config" / "permissions"
    perm_dir.mkdir(parents=True, exist_ok=True)
    (perm_dir / "read_only.txt").write_text(
        "echo\nls\nsed\nfx test\ngit status\ngit diff\n",
        encoding="utf-8",
    )
    (perm_dir / "local_changes.txt").write_text("sed -i\n", encoding="utf-8")
    (perm_dir / "never_allow.txt").write_text(
        "git clean -f\ngit reset --hard\ngit push --force\nrm -rf /\n",
        encoding="utf-8",
    )
    for name in (
        "external_changes.txt",
        "device_ops.txt",
        "cache_destruction.txt",
        "batch_execution.txt",
    ):
        (perm_dir / name).write_text("", encoding="utf-8")
    return fuchsia_dir


def _run_setup_isolated(
    args_list: list[str],
    config_path: pathlib.Path,
    state_dir: pathlib.Path,
    tpath: pathlib.Path | None = None,
) -> int:
    parser = argparse.ArgumentParser()
    setup.add_arguments(parser)
    cmd_args = [
        "--config",
        str(config_path),
        "--state-dir",
        str(state_dir),
        "--no-git-hooks",
    ] + args_list
    parsed = parser.parse_args(cmd_args)
    root_ctx = (
        mock.patch.object(
            paths,
            "find_fuchsia_dir",
            return_value=_ensure_hermetic_fuchsia_dir(tpath),
        )
        if tpath is not None
        else contextlib.nullcontext()
    )
    with (
        _IO_LOCK,
        mock.patch("sys.stdout", new_callable=io.StringIO),
        root_ctx,
        mock.patch.object(
            githooks,
            "uninstall_git_hooks",
            return_value=githooks.HookOperationResult(modified=[], failed=[]),
        ),
        mock.patch.object(services, "restart_daemons"),
    ):
        return setup.run(parsed)


class OfflineConfigRegexTest(BaseTestCase):
    """Offline tests for config.json regex matching and PreToolUse hooks."""

    def test_compound_command_injection_blocked_by_hook(self) -> None:
        """Verify compound command injections are denied by PreToolUse hook."""
        tpath = self.test_dir
        cfg_path = tpath / ".gemini" / "config" / "config.json"
        state_dir = tpath / "state"
        exit_code = _run_setup_isolated(
            ["--profile", "read-only"], cfg_path, state_dir, tpath
        )
        self.assertEqual(exit_code, 0)

        for cmd, expected in (
            (
                "echo compound_check && sed -i 's/a/b/g' /tmp/sample.txt",
                "force_ask",
            ),
            ("git status ; rm -rf /", "deny"),
            ("ls | git clean -fd", "deny"),
        ):
            with self.subTest(cmd=cmd):
                res = evaluate_command_hooks(cmd, config_path=cfg_path)
                self.assertEqual(res.decision, expected, cmd)

    def test_benign_redirection_and_compound_commands_allowed(self) -> None:
        """Verify benign redirection and compound commands pass hooks."""
        tpath = self.test_dir
        cfg_path = tpath / ".gemini" / "config" / "config.json"
        state_dir = tpath / "state"
        exit_code = _run_setup_isolated(
            ["--profile", "read-only"], cfg_path, state_dir, tpath
        )
        self.assertEqual(exit_code, 0)

        policy = GrantPolicyConfig.from_files(config_path=cfg_path)
        for cmd in (
            "fx test //tools/agents 2>&1",
            "git status && git diff",
        ):
            with self.subTest(cmd=cmd):
                self.assertTrue(
                    any(
                        matches_grant_rule(cmd, rule)
                        for rule in policy.allow_grants
                    ),
                    f"Expected '{cmd}' to match allow grants in config.json",
                )
                res = evaluate_command_hooks(cmd, config_path=cfg_path)
                self.assertEqual(res.decision, "allow", cmd)


class LiveAgentPermissionsE2ETest(unittest.TestCase):
    """Live contract tests for agent CLI permissions and revocation."""

    def _get_agent_bin_or_skip(self) -> str:
        agent_bin = os.environ.get("AGENT_CLI_PATH", "")
        if not agent_bin or not pathlib.Path(agent_bin).is_file():
            self.skipTest(
                f"Agent CLI binary not found at '{agent_bin}'. "
                "Set AGENT_CLI_PATH to run live end-to-end tests."
            )
        return agent_bin

    @staticmethod
    def _init_sandbox_repo(repo_dir: pathlib.Path) -> None:
        git_env = os.environ | {
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        subprocess.run(
            ["git", "init", "--template=", "-b", "main"],
            cwd=repo_dir,
            check=True,
            capture_output=True,
            env=git_env,
        )

    @contextlib.contextmanager
    def _sandbox(
        self,
    ) -> Iterator[
        tuple[
            pathlib.Path,
            pathlib.Path,
            pathlib.Path,
            pathlib.Path,
            pathlib.Path,
        ]
    ]:
        with tempfile.TemporaryDirectory() as td:
            tpath = pathlib.Path(td)
            workspace_dir = tpath / "workspace"
            workspace_dir.mkdir()
            self._init_sandbox_repo(workspace_dir)
            gemini_dir = tpath / ".gemini"
            cfg_path = gemini_dir / "config" / "config.json"
            state_dir = tpath / "state"
            sample_file = workspace_dir / "sample.txt"
            sample_file.write_text("hello world\n", encoding="utf-8")
            yield workspace_dir, gemini_dir, cfg_path, state_dir, sample_file

    def _execute_and_evaluate(
        self,
        test_cases: list[CommandTestCase],
        gemini_dir: pathlib.Path,
        workspace_dir: pathlib.Path,
        agent_bin: str,
        extra_agent_args: list[str] | None = None,
        timeout_seconds: int = 120,
        failure_prefix: str = "Enforcement failed",
    ) -> None:
        commands_list_text = "\n".join(
            f"{i+1}. `{tc.command}`" for i, tc in enumerate(test_cases)
        )
        prompt = (
            "We are performing an automated permission verification test.\n"
            "Please use the `run_command` tool to execute each of the "
            "following shell commands one by one in order:\n\n"
            f"{commands_list_text}\n\n"
            "Do NOT retry, inspect, or work around any command that fails or "
            "is denied by a pre-tool hook or permission policy; attempt each "
            "command at most once and immediately proceed to the next "
            "command.\n"
            "After attempting all commands, provide a final summary of which "
            "commands executed successfully and which ones were "
            "denied/blocked by permission policy."
        )
        agent_cmd = [
            agent_bin,
            f"-gemini_dir={gemini_dir}",
            f"-workspace_id={workspace_dir}",
            "-output-format=json",
        ]
        if extra_agent_args:
            agent_cmd.extend(extra_agent_args)
        try:
            result = subprocess.run(
                agent_cmd,
                input=prompt,
                text=True,
                capture_output=True,
                timeout=timeout_seconds,
                cwd=workspace_dir,
            )
        except subprocess.TimeoutExpired:
            self.fail(f"Agent CLI timed out after {timeout_seconds}s")

        matched_files = [
            str(p) for p in gemini_dir.rglob("transcript*.jsonl") if p.is_file()
        ]
        self.assertTrue(
            matched_files,
            f"No transcript logs generated. Stderr: {result.stderr}",
        )
        transcript_file = max(matched_files, key=os.path.getmtime)
        tool_events = _extract_command_events(transcript_file)
        passed, passed_count, total_count, summary = evaluate_test_cases(
            test_cases, tool_events
        )
        self.assertTrue(
            passed,
            f"{failure_prefix} ({passed_count}/{total_count} passed):\n\n"
            f"{summary}",
        )

    def _assert_profile_enforcement(
        self,
        setup_args: list[str],
        specs: list[tuple[str, str, str]],
        extra_agent_args: list[str] | None = None,
    ) -> None:
        agent_bin = self._get_agent_bin_or_skip()
        with self._sandbox() as (
            workspace_dir,
            gemini_dir,
            cfg_path,
            state_dir,
            sample_file,
        ):
            exit_code = _run_setup_isolated(setup_args, cfg_path, state_dir)
            self.assertEqual(exit_code, 0, f"setup.run failed for {setup_args}")
            self.assertTrue(
                cfg_path.is_file(),
                f"config.json was not created at {cfg_path}",
            )

            test_cases = [
                CommandTestCase(
                    command=cmd.format(
                        sample_file=sample_file, workspace_dir=workspace_dir
                    ),
                    expected_status=expected,
                    description=desc,
                )
                for cmd, expected, desc in specs
            ]
            self._execute_and_evaluate(
                test_cases=test_cases,
                gemini_dir=gemini_dir,
                workspace_dir=workspace_dir,
                agent_bin=agent_bin,
                extra_agent_args=extra_agent_args,
                failure_prefix=f"Enforcement failed for {setup_args}",
            )

    def test_read_only_profile(self) -> None:
        """Verify boundary command permissions for read-only profile."""
        self._assert_profile_enforcement(
            ["--profile", "read-only"],
            [
                ("fx help", "ALLOW", "Allowed read-only inspection"),
                (
                    "sed -i 's/hello/world/g' {sample_file}",
                    "DENY",
                    "Denied in-place local mutation",
                ),
                (
                    "echo compound_check && "
                    "sed -i 's/hello/world/g' {sample_file}",
                    "DENY",
                    "Denied compound command containing in-place sed",
                ),
                (
                    "git push --dry-run",
                    "DENY",
                    "Denied upstream external change",
                ),
            ],
        )

    def test_local_changes_profile(self) -> None:
        """Verify boundary command permissions for local-changes profile."""
        self._assert_profile_enforcement(
            ["--profile", "local-changes"],
            [
                ("fx help", "ALLOW", "Allowed read-only inspection"),
                (
                    "sed -i 's/hello/world/g' {sample_file}",
                    "ALLOW",
                    "Allowed in-place local mutation",
                ),
                (
                    "git push --dry-run",
                    "DENY",
                    "Denied upstream external change",
                ),
                (
                    "git clean -fdx",
                    "DENY",
                    "Denied destructive clean (never allow)",
                ),
            ],
        )

    def test_external_changes_profile(self) -> None:
        """Verify boundary command permissions for external-changes profile."""
        self._assert_profile_enforcement(
            ["--profile", "external-changes"],
            [
                ("fx help", "ALLOW", "Allowed read-only inspection"),
                (
                    "sed -i 's/hello/world/g' {sample_file}",
                    "ALLOW",
                    "Allowed in-place local mutation",
                ),
                (
                    "git push --dry-run",
                    "ALLOW",
                    "Allowed upstream external change",
                ),
                (
                    "git clean -fdx",
                    "DENY",
                    "Denied destructive clean (never allow)",
                ),
            ],
        )

    def test_ad_hoc_grants(self) -> None:
        """Verify ad-hoc --allow and --deny flags are enforced by agent CLI."""
        self._assert_profile_enforcement(
            [
                "--profile",
                "read-only",
                "-a",
                "echo AD_HOC_ALLOWED_TEST",
                "-d",
                "echo AD_HOC_DENIED_TEST",
            ],
            [
                (
                    "echo AD_HOC_ALLOWED_TEST",
                    "ALLOW",
                    "Ad-hoc command explicitly added via --allow",
                ),
                (
                    "echo AD_HOC_DENIED_TEST",
                    "DENY",
                    "Ad-hoc command explicitly added via --deny",
                ),
            ],
        )

    def test_turbo_mode(self) -> None:
        """Verify Turbo mode still enforces never-allow and deny rules."""
        self._assert_profile_enforcement(
            [
                "--profile",
                "local-changes",
                "-d",
                "whoami",
                "-d",
                "echo TURBO_AD_HOC_DENIED",
            ],
            [
                (
                    "echo TURBO_UNLISTED_ALLOWED",
                    "ALLOW",
                    "Allowed command in Turbo mode",
                ),
                (
                    "whoami",
                    "DENY",
                    "Denied single-word --deny rule in Turbo mode",
                ),
                (
                    "git clean -fdx",
                    "DENY",
                    "Denied never-allow rule even in Turbo mode",
                ),
                (
                    "echo TURBO_AD_HOC_DENIED",
                    "DENY",
                    "Denied multi-word --deny rule even in Turbo mode",
                ),
            ],
            extra_agent_args=["-dangerously-skip-permissions"],
        )

    def test_flag_matching_variants(self) -> None:
        """Verify global flags, env vars, non-inplace sed, and force-push."""
        self._assert_profile_enforcement(
            ["--profile", "read-only", "-a", "git push"],
            [
                (
                    "git -C {workspace_dir} status",
                    "ALLOW",
                    "Allowed git command with global -C flag",
                ),
                (
                    "FOO_TEST_VAR=1 fx help",
                    "ALLOW",
                    "Allowed fx command with leading env var",
                ),
                (
                    "fx --dir out/default help",
                    "ALLOW",
                    "Allowed fx command with global --dir flag",
                ),
                (
                    "sed 's/hello/world/g' {sample_file}",
                    "ALLOW",
                    "Allowed read-only stream sed without -i",
                ),
                (
                    "git push origin main --force --dry-run",
                    "DENY",
                    "Denied git push --force even when git push is allowed",
                ),
                (
                    "git -C {workspace_dir} clean -fdx",
                    "DENY",
                    "Denied git -C <dir> clean -fdx (never allow)",
                ),
            ],
        )

    def test_permission_revocation_and_reset(self) -> None:
        """Verify managed permissions can be revoked and purged via --reset."""
        agent_bin = self._get_agent_bin_or_skip()
        with self._sandbox() as (
            workspace_dir,
            gemini_dir,
            cfg_path,
            state_dir,
            sample_file,
        ):
            # 1. Configure local-changes and verify sed -i is ALLOWED
            exit_code = _run_setup_isolated(
                ["--profile", "local-changes"], cfg_path, state_dir
            )
            self.assertEqual(exit_code, 0)

            test_case_before = [
                CommandTestCase(
                    command=f"sed -i 's/hello/world/g' {sample_file}",
                    expected_status="ALLOW",
                    description="sed -i allowed under local-changes profile",
                ),
            ]
            self._execute_and_evaluate(
                test_cases=test_case_before,
                gemini_dir=gemini_dir,
                workspace_dir=workspace_dir,
                agent_bin=agent_bin,
                failure_prefix="sed -i not allowed under local-changes",
            )

            # 2. Reset configuration and verify sed -i is now DENIED
            exit_code = _run_setup_isolated(["--reset"], cfg_path, state_dir)
            self.assertEqual(exit_code, 0)

            test_case_after = [
                CommandTestCase(
                    command=f"sed -i 's/hello/world/g' {sample_file}",
                    expected_status="DENY",
                    description="sed -i denied after reset clears grants",
                ),
            ]
            self._execute_and_evaluate(
                test_cases=test_case_after,
                gemini_dir=gemini_dir,
                workspace_dir=workspace_dir,
                agent_bin=agent_bin,
                failure_prefix="sed -i not denied after reset",
            )


if __name__ == "__main__":
    parallel_main()
