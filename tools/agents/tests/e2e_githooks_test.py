#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""End-to-end Git hooks integration test suite validating real platform tools."""

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Collection, Mapping

from agents.lib import githooks, paths


def _find_bundled_tool(relative_path: str) -> pathlib.Path | None:
    """Locates a runtime dependency tool from the Fuchsia checkout.

    Args:
        relative_path: Relative path from the Fuchsia root to the tool file.

    Returns:
        Resolved `pathlib.Path` to the tool if found, or `None` if the checkout
        root cannot be located or the tool file does not exist.
    """
    try:
        fuchsia_dir = paths.find_fuchsia_dir(
            pathlib.Path(__file__).resolve().parent
        )
    except RuntimeError:
        return None
    candidate = fuchsia_dir / relative_path
    if candidate.is_file():
        return candidate
    return None


_MOCK_FX_WRAPPER_TEMPLATE = r"""#!/usr/bin/env fuchsia-vendored-python
import os
import sys
import subprocess
from pathlib import Path

args = sys.argv[1:]
if not args:
    sys.exit(0)

if args[0] == "format-code":
    files = [f for a in args if a.startswith("--files=") for f in a[8:].split(",") if f]
    json_fmt = os.environ.get("REAL_JSON_FMT")

    json_files = []
    for f in files:
        p = Path(f)
        if not p.is_file():
            continue
        ext = p.suffix.lower()
        if ext in (".json", ".config"):
            json_files.append(str(p))

    failed = False
    for jf in json_files:
        if json_fmt and Path(json_fmt).is_file():
            res = subprocess.run(
                [sys.executable, json_fmt, "--no-sort-keys", "--quiet", jf],
                capture_output=True,
                text=True,
            )
            if res.returncode != 0:
                sys.stderr.write(res.stderr or res.stdout)
                failed = True

    if failed:
        sys.exit(1)
    sys.exit(0)

if args[0] == "lint":
    # `fx lint --fix` delegates to `shac fix`, which skips formatter checks and
    # exits 0 even when findings remain. Emulate that so tests can verify the
    # separate check pass is what actually gates the commit.
    if "--fix" in args and os.environ.get("SHAC_FIX_SEMANTICS") == "1":
        sys.exit(0)
    if os.environ.get("FAIL_MOCK_LINT") == "1":
        sys.stderr.write("Mock lint failure on modified files\n")
        sys.exit(1)
    sys.exit(0)
"""

_MOCK_JIRI_DISPATCHER_TEMPLATE = (
    "#!/bin/sh\n"
    'HOOK_NAME="{hook_name}"\n'
    'GIT_HOOKS_DIR="$(git rev-parse --git-path hooks 2>/dev/null || echo "$PWD/.git/hooks")"\n'
    'HOOK_D="$GIT_HOOKS_DIR/${{HOOK_NAME}}.d"\n'
    'if [ -d "$HOOK_D" ]; then\n'
    '  for script in "$HOOK_D"/*; do\n'
    '    [ -f "$script" ] || continue\n'
    '    case "$script" in\n'
    "      *~ | *.bak | *.bak.* | *.tmp | *.swp | *.sample | *.disabled) continue ;;\n"
    "    esac\n"
    '    if [ -x "$script" ]; then\n'
    '      "$script" "$@" || exit $?\n'
    "    fi\n"
    "  done\n"
    "fi\n"
    "exit 0\n"
)


def _init_git_repo(
    repo_dir: pathlib.Path,
    user_name: str,
    user_email: str,
    install_jiri_dispatchers: bool = True,
) -> None:
    """Initializes a git repository with standard test config and mock dispatchers.

    Args:
        repo_dir: Directory where the git repository should be initialized.
        user_name: Git `user.name` configuration value.
        user_email: Git `user.email` configuration value.
        install_jiri_dispatchers: Whether to install mock Jiri hook dispatchers.
    """
    git_env = os.environ | {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
    }
    for command in (
        ["git", "init", "--template=", "-b", "main"],
        ["git", "config", "user.name", user_name],
        ["git", "config", "user.email", user_email],
        ["git", "config", "commit.gpgsign", "false"],
    ):
        subprocess.run(
            command,
            cwd=repo_dir,
            check=True,
            capture_output=True,
            env=git_env,
        )

    if install_jiri_dispatchers:
        hooks_dir = repo_dir / ".git" / "hooks"
        hooks_dir.mkdir(parents=True, exist_ok=True)
        for hook_name in ("pre-commit", "commit-msg"):
            hook_file = hooks_dir / hook_name
            hook_file.write_text(
                _MOCK_JIRI_DISPATCHER_TEMPLATE.format(hook_name=hook_name),
                encoding="utf-8",
            )
            hook_file.chmod(0o755)


class RealHooksWorkspaceFixture:
    """An isolated Git workspace configured with real platform tools."""

    def __init__(self) -> None:
        """Initializes clean and hook-enabled workspace templates."""
        self.commit_msg_checker = _find_bundled_tool(
            "scripts/shac/commit_msg_checker.py"
        )
        self.json_fmt = _find_bundled_tool("scripts/shac/json_format.py")

        self.template_dir_clean = pathlib.Path(
            tempfile.mkdtemp(prefix="real_hooks_clean_template_")
        )
        self.template_dir_with_hooks = pathlib.Path(
            tempfile.mkdtemp(prefix="real_hooks_installed_template_")
        )
        try:
            self._setup_clean_template()
            self._setup_with_hooks_template()
        except Exception:
            self.cleanup()
            raise

    def cleanup(self) -> None:
        """Removes temporary template directories created by this fixture."""
        if self.template_dir_clean.exists():
            shutil.rmtree(self.template_dir_clean, ignore_errors=True)
        if self.template_dir_with_hooks.exists():
            shutil.rmtree(self.template_dir_with_hooks, ignore_errors=True)

    def _setup_clean_template(self) -> None:
        root = self.template_dir_clean
        (root / ".jiri_root").mkdir(parents=True, exist_ok=True)
        (root / ".fx-root").touch(exist_ok=True)

        git_env = os.environ | {
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
        }

        _init_git_repo(root, "Test Author", "test@example.com")

        (root / ".gitignore").write_text(
            "/.fx-root\n/bin/\n/scripts/\n/tools/\n/vendor/\n/.worktrees/\n/worktree-*/\n",
            encoding="utf-8",
        )

        src_dir = root / "src"
        src_dir.mkdir(parents=True, exist_ok=True)
        (src_dir / "foo.json").write_text(
            '{\n    "name": "foo"\n}\n', encoding="utf-8"
        )

        subprocess.run(
            ["git", "add", ".gitignore", "src/"],
            cwd=root,
            check=True,
            env=git_env,
        )
        subprocess.run(
            [
                "git",
                "commit",
                "-m",
                "[root] Initial baseline\n\nBug: 123\nTest: none\nChange-Id: I0000000000000000000000000000000000000001\n",
            ],
            cwd=root,
            check=True,
            env=git_env,
        )

        vendor_dir = root / "vendor" / "google"
        vendor_dir.mkdir(parents=True, exist_ok=True)
        _init_git_repo(vendor_dir, "Vendor Author", "vendor@example.com")

        (vendor_dir / "lib.json").write_text(
            '{\n    "name": "lib"\n}\n', encoding="utf-8"
        )
        subprocess.run(
            ["git", "add", "lib.json"], cwd=vendor_dir, check=True, env=git_env
        )
        subprocess.run(
            [
                "git",
                "commit",
                "-m",
                "[vendor] Baseline\n\nBug: 123\nTest: none\nChange-Id: I0000000000000000000000000000000000000002\n",
            ],
            cwd=vendor_dir,
            check=True,
            env=git_env,
        )

        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True, exist_ok=True)
        bin_dir = root / "bin"
        bin_dir.mkdir(parents=True, exist_ok=True)

        fx_script = bin_dir / "fx"
        fx_script.write_text(_MOCK_FX_WRAPPER_TEMPLATE, encoding="utf-8")
        fx_script.chmod(0o755)

        python_bin = scripts_dir / "fuchsia-vendored-python"
        python_bin.symlink_to(sys.executable)

        shac_dir = scripts_dir / "shac"
        shac_dir.mkdir(parents=True, exist_ok=True)
        if (
            self.commit_msg_checker is not None
            and self.commit_msg_checker.is_file()
        ):
            (shac_dir / "commit_msg_checker.py").symlink_to(
                self.commit_msg_checker
            )

        tools_dir = root / "tools"
        tools_dir.mkdir(parents=True, exist_ok=True)
        fuchsia_root = paths.find_fuchsia_dir(
            pathlib.Path(__file__).resolve().parent
        )
        shutil.copytree(
            fuchsia_root / "tools" / "agents",
            tools_dir / "agents",
            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
        )

    def _setup_with_hooks_template(self) -> None:
        shutil.copytree(
            self.template_dir_clean,
            self.template_dir_with_hooks,
            dirs_exist_ok=True,
            symlinks=True,
        )
        install_result = githooks.install_git_hooks(
            self.template_dir_with_hooks
        )
        if install_result.failed:
            raise RuntimeError(
                f"Failed to pre-install dev hooks in template: {install_result.failed}"
            )

    def create_isolated_workspace(
        self, prefix: str = "real_hooks_test_", with_hooks: bool = True
    ) -> pathlib.Path:
        """Creates an isolated workspace copy from the prepared template.

        Args:
            prefix: Temporary directory name prefix.
            with_hooks: Whether to copy the template with pre-installed hooks.

        Returns:
            Path to the newly created temporary workspace directory.
        """
        temp_dir = pathlib.Path(tempfile.mkdtemp(prefix=prefix))
        if with_hooks:
            source_template = self.template_dir_with_hooks
        else:
            source_template = self.template_dir_clean
        shutil.copytree(
            source_template, temp_dir, dirs_exist_ok=True, symlinks=True
        )
        return temp_dir


@unittest.skipUnless(
    shutil.which("git") is not None,
    "git binary not available in test environment",
)
class E2EGitHooksTest(unittest.TestCase):
    """End-to-end test cases verifying real format-code and commit_msg_checker."""

    fixture: RealHooksWorkspaceFixture

    @classmethod
    def setUpClass(cls) -> None:
        if shutil.which("git") is None:
            raise unittest.SkipTest(
                "git binary not available in test environment"
            )
        super().setUpClass()
        cls.fixture = RealHooksWorkspaceFixture()
        cls.addClassCleanup(cls.fixture.cleanup)
        if cls.fixture.commit_msg_checker is None:
            raise unittest.SkipTest(
                "commit_msg_checker.py could not be located in test dependencies or checkout"
            )
        if cls.fixture.json_fmt is None:
            raise unittest.SkipTest(
                "json_format.py could not be located in test dependencies or checkout"
            )

    def _run_git(
        self,
        *args: str,
        cwd: pathlib.Path,
        check: bool = False,
        extra_env: Mapping[str, str] | None = None,
        remove_env: Collection[str] = (),
    ) -> subprocess.CompletedProcess[str]:
        try:
            top = paths.find_fuchsia_dir(cwd)
        except RuntimeError:
            try:
                completed_process = subprocess.run(
                    ["git", "rev-parse", "--git-common-dir"],
                    cwd=cwd,
                    capture_output=True,
                    text=True,
                    check=True,
                )
            except (subprocess.CalledProcessError, OSError):
                top = cwd
            else:
                common = pathlib.Path(completed_process.stdout.strip())
                if not common.is_absolute():
                    common = (cwd / common).resolve()
                try:
                    top = paths.find_fuchsia_dir(common.parent)
                except RuntimeError:
                    top = cwd

        bin_dir = str(top / "bin")
        scripts_dir = str(top / "scripts")
        if self.fixture.json_fmt is not None:
            json_fmt_env = {"REAL_JSON_FMT": str(self.fixture.json_fmt)}
        else:
            json_fmt_env = {}

        excluded_vars = {
            "FUCHSIA_SKIP_HOOKS",
            "ANTIGRAVITY_AGENT",
            "GEMINI_CLI",
            "ANTIGRAVITY_EDITOR_APP_ROOT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        }
        base_env = {
            key: value
            for key, value in os.environ.items()
            if key not in excluded_vars
        }
        env = (
            base_env
            | {
                "PATH": f"{bin_dir}:{scripts_dir}:{os.environ.get('PATH', '')}",
                "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_CONFIG_NOSYSTEM": "1",
                "FUCHSIA_DIR": str(top),
            }
            | json_fmt_env
        )
        for key in remove_env:
            env.pop(key, None)
        if extra_env:
            env.update(extra_env)

        return subprocess.run(
            ["git", *args],
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            check=check,
            timeout=30,
        )

    def test_e2e_pre_commit_formats_and_restages(self) -> None:
        """Verifies that pre-commit auto-formats and re-stages for both human and agent commits."""
        workspace = self.fixture.create_isolated_workspace()
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        foo_json = workspace / "src" / "foo.json"
        unformatted_content = '{\n  "unformatted":   true,\n "key": "value" \n}'
        foo_json.write_text(unformatted_content, encoding="utf-8")
        self._run_git("add", "src/foo.json", cwd=workspace, check=True)

        commit_message = (
            "[test] Multi-language format\n\n"
            "Bug: 1234\n"
            "Test: none\n"
            "Change-Id: I0000000000000000000000000000000000000020\n"
        )
        expected_formatted = (
            '{\n    "unformatted": true,\n    "key": "value"\n}\n'
        )

        # 1. Human developer commit: pre-commit hook runs, auto-formats and re-stages
        human_result = self._run_git(
            "commit", "-m", commit_message, cwd=workspace
        )
        self.assertEqual(
            human_result.returncode,
            0,
            f"Human commit should succeed with auto-formatting: {human_result.stderr}\n{human_result.stdout}",
        )
        show_json_human = self._run_git(
            "show", "HEAD:src/foo.json", cwd=workspace, check=True
        )
        self.assertEqual(show_json_human.stdout, expected_formatted)
        self.assertEqual(
            foo_json.read_text(encoding="utf-8"), expected_formatted
        )

        # 2. Stage another unformatted file for agent commit
        bar_json = workspace / "src" / "bar.json"
        bar_json.write_text(unformatted_content, encoding="utf-8")
        self._run_git("add", "src/bar.json", cwd=workspace, check=True)

        # Agent commit: pre-commit hook runs, auto-formats and re-stages
        agent_result = self._run_git(
            "commit",
            "-m",
            commit_message,
            cwd=workspace,
            extra_env={"ANTIGRAVITY_AGENT": "1"},
        )
        self.assertEqual(
            agent_result.returncode,
            0,
            f"Agent commit should succeed with auto-formatting: {agent_result.stderr}\n{agent_result.stdout}",
        )

        show_json = self._run_git(
            "show", "HEAD:src/bar.json", cwd=workspace, check=True
        )
        self.assertEqual(show_json.stdout, expected_formatted)
        self.assertEqual(
            bar_json.read_text(encoding="utf-8"), expected_formatted
        )

        status_result = self._run_git(
            "status", "--porcelain", cwd=workspace, check=True
        )
        self.assertEqual(status_result.stdout.strip(), "")

    def test_e2e_pre_commit_blocks_partial_staging_conflict(self) -> None:
        """Verifies that a partially staged file with formatting violations blocks commits."""
        workspace = self.fixture.create_isolated_workspace()
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        foo_json = workspace / "src" / "foo.json"
        staged_content = '{\n  "unformatted":   true,\n "key": "foo" \n}'
        foo_json.write_text(staged_content, encoding="utf-8")
        self._run_git("add", "src/foo.json", cwd=workspace, check=True)

        unstaged_content = (
            '{\n  "unformatted":   true,\n "key": "foo",\n "extra": 1 \n}'
        )
        foo_json.write_text(unstaged_content, encoding="utf-8")

        commit_message = (
            "[test] Partial staging conflict\n\n"
            "Bug: 1234\n"
            "Test: none\n"
            "Change-Id: I0000000000000000000000000000000000000031\n"
        )
        for actor, extra_env in (
            ("human", None),
            ("agent", {"ANTIGRAVITY_AGENT": "1"}),
        ):
            with self.subTest(actor=actor):
                result = self._run_git(
                    "commit",
                    "-m",
                    commit_message,
                    cwd=workspace,
                    extra_env=extra_env,
                )
                self.assertEqual(
                    result.returncode,
                    1,
                    "Commit must fail when partially staged file has formatting violations",
                )
                self.assertEqual(
                    foo_json.read_text(encoding="utf-8"), unstaged_content
                )
                show_index = self._run_git(
                    "show", ":src/foo.json", cwd=workspace, check=True
                )
                self.assertEqual(show_index.stdout, staged_content)
                self.assertIn(
                    "Formatting or check failure on partially staged files",
                    result.stderr + result.stdout,
                )

    def test_e2e_commit_msg_enforcement(self) -> None:
        """Verifies commit_msg_checker runs: warnings pass for humans, fail closed for agents."""
        workspace = self.fixture.create_isolated_workspace()
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        warning_message = (
            "[tools][agents] This commit message subject is excessively long and exceeds the sixty-five character limit\n\n"
            "This line is intentionally crafted to exceed the maximum seventy-two character limit for body wrapping.\n\n"
            "Bug: 1234\n"
            "Test: none\n"
            "Change-Id: I0000000000000000000000000000000000000040\n"
            "TAG=agy\n"
        )

        with self.subTest(actor="human"):
            # Human developer (is_agent=False): commit passes with advisory warnings
            human_result = self._run_git(
                "commit", "--allow-empty", "-m", warning_message, cwd=workspace
            )
            self.assertEqual(
                human_result.returncode,
                0,
                f"Human commit should pass with advisory warnings: {human_result.stderr}",
            )
            output_human = human_result.stdout + human_result.stderr
            self.assertIn("Subject line exceeds 65 characters", output_human)
            self.assertIn("exceeds 72 characters", output_human)

        with self.subTest(actor="agent"):
            # Agent developer (ANTIGRAVITY_AGENT=1): commit fails closed on style warnings
            agent_result = self._run_git(
                "commit",
                "--allow-empty",
                "-m",
                warning_message,
                cwd=workspace,
                extra_env={"ANTIGRAVITY_AGENT": "1"},
            )
            self.assertEqual(
                agent_result.returncode,
                1,
                "Agent commit should fail strictly on style violations",
            )
            output_agent = agent_result.stdout + agent_result.stderr
            self.assertIn("Subject line exceeds 65 characters", output_agent)

    def test_e2e_worktree_execution(self) -> None:
        """Verifies that hooks function properly in a linked Git worktree."""
        workspace = self.fixture.create_isolated_workspace()
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        worktree_temp = pathlib.Path(tempfile.mkdtemp(prefix="e2e_wt_"))
        self.addCleanup(shutil.rmtree, worktree_temp, ignore_errors=True)
        worktree_dir = worktree_temp / "wt1"
        self._run_git(
            "worktree",
            "add",
            str(worktree_dir),
            "-b",
            "wt-branch",
            cwd=workspace,
            check=True,
        )

        worktree_json = worktree_dir / "src" / "foo.json"
        unformatted_content = '{\n  "unformatted":   true,\n "key": "value" \n}'
        worktree_json.write_text(unformatted_content, encoding="utf-8")
        self._run_git("add", "src/foo.json", cwd=worktree_dir, check=True)

        commit_message = (
            "[wt] Worktree commit\n\n"
            "Bug: 1234\n"
            "Test: none\n"
            "Change-Id: I0000000000000000000000000000000000000060\n"
        )
        result = self._run_git(
            "commit",
            "-m",
            commit_message,
            cwd=worktree_dir,
            remove_env=("FUCHSIA_DIR",),
        )
        self.assertEqual(
            result.returncode,
            0,
            f"Worktree commit failed: {result.stderr}\n{result.stdout}",
        )

        expected_formatted = (
            '{\n    "unformatted": true,\n    "key": "value"\n}\n'
        )
        show_result = self._run_git(
            "show", "HEAD:src/foo.json", cwd=worktree_dir, check=True
        )
        self.assertEqual(show_result.stdout, expected_formatted)
        self.assertEqual(
            worktree_json.read_text(encoding="utf-8"), expected_formatted
        )
        status_result = self._run_git(
            "status", "--porcelain", cwd=worktree_dir, check=True
        )
        self.assertEqual(status_result.stdout.strip(), "")

    def test_e2e_commit_blocks_on_lint_failure(self) -> None:
        """Verifies that lint failures block both human and agent commits."""
        workspace = self.fixture.create_isolated_workspace(with_hooks=True)
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        target_file = workspace / "src" / "foo.json"
        target_file.write_text('{\n    "valid": true\n}\n', encoding="utf-8")
        self._run_git("add", "src/foo.json", cwd=workspace, check=True)

        commit_message = (
            "[root] Valid commit message\n\n"
            "Bug: 1234\n"
            "Test: none\n"
            "Change-Id: I0000000000000000000000000000000000000077\n"
        )

        with self.subTest(actor="human_failing_lint"):
            # Human developer commit with a failing linter is blocked.
            human_result = self._run_git(
                "commit",
                "-m",
                commit_message,
                cwd=workspace,
                extra_env={"FAIL_MOCK_LINT": "1"},
            )
            self.assertNotEqual(human_result.returncode, 0)
            self.assertIn(
                "Mock lint failure on modified files",
                human_result.stderr + human_result.stdout,
            )

        with self.subTest(actor="agent_failing_lint"):
            # AI coding agent commit with a failing linter is blocked.
            agent_result = self._run_git(
                "commit",
                "-m",
                commit_message,
                cwd=workspace,
                extra_env={"GEMINI_CLI": "1", "FAIL_MOCK_LINT": "1"},
            )
            self.assertNotEqual(agent_result.returncode, 0)
            self.assertIn(
                "Mock lint failure on modified files",
                agent_result.stderr + agent_result.stdout,
            )

        with self.subTest(actor="passing_lint"):
            # Commit succeeds once the linters pass.
            ok_result = self._run_git(
                "commit",
                "-m",
                commit_message,
                cwd=workspace,
            )
            self.assertEqual(ok_result.returncode, 0)

    def test_e2e_commit_blocks_when_fix_pass_exits_zero(self) -> None:
        """Verifies the check pass gates even when `--fix` reports success.

        `fx lint --fix` delegates to `shac fix`, which skips formatter checks
        and exits 0 even when findings remain. Relying on it alone silently
        disabled the lint gate, so the adapter runs a separate check pass.
        """
        workspace = self.fixture.create_isolated_workspace(with_hooks=True)
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        target_file = workspace / "src" / "foo.json"
        target_file.write_text('{\n    "valid": true\n}\n', encoding="utf-8")
        self._run_git("add", "src/foo.json", cwd=workspace, check=True)

        commit_message = "[test] Lint gate regression\n\nBug: None\nTest: e2e"
        result = self._run_git(
            "commit",
            "-m",
            commit_message,
            cwd=workspace,
            extra_env={
                "GEMINI_CLI": "1",
                "FAIL_MOCK_LINT": "1",
                "SHAC_FIX_SEMANTICS": "1",
            },
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "Mock lint failure on modified files", result.stderr + result.stdout
        )

    def test_e2e_multi_repo_setup_and_reset(self) -> None:
        """Verifies that install_git_hooks configures root and vendor repos, and uninstall removes them."""
        workspace = self.fixture.create_isolated_workspace(with_hooks=False)
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)

        repos = (workspace, workspace / "vendor" / "google")

        with self.subTest(phase="initial_status"):
            status_init = githooks.get_git_hooks_status(workspace)
            self.assertEqual(status_init.total_repos, 2)
            self.assertEqual(status_init.configured_repos, 0)

        with self.subTest(phase="install_hooks"):
            install_result = githooks.install_git_hooks(workspace)
            self.assertSequenceEqual(install_result.failed, ())
            self.assertGreater(len(install_result.modified), 0)

            for repo in repos:
                hooks_dir = repo / ".git" / "hooks"
                self.assertTrue(
                    (
                        hooks_dir / "pre-commit.d" / "20-fuchsia-agent.sh"
                    ).is_file()
                )
                self.assertTrue(
                    (
                        hooks_dir / "commit-msg.d" / "20-fuchsia-agent.sh"
                    ).is_file()
                )

            status_installed = githooks.get_git_hooks_status(workspace)
            self.assertEqual(status_installed.total_repos, 2)
            self.assertEqual(status_installed.configured_repos, 2)

        with self.subTest(phase="uninstall_hooks"):
            uninstall_result = githooks.uninstall_git_hooks(workspace)
            self.assertSequenceEqual(uninstall_result.failed, ())
            self.assertGreater(len(uninstall_result.modified), 0)

            for repo in repos:
                hooks_dir = repo / ".git" / "hooks"
                self.assertFalse(
                    (
                        hooks_dir / "pre-commit.d" / "20-fuchsia-agent.sh"
                    ).exists()
                )
                self.assertFalse(
                    (
                        hooks_dir / "commit-msg.d" / "20-fuchsia-agent.sh"
                    ).exists()
                )

            status_uninstalled = githooks.get_git_hooks_status(workspace)
            self.assertEqual(status_uninstalled.total_repos, 2)
            self.assertEqual(status_uninstalled.configured_repos, 0)


if __name__ == "__main__":
    unittest.main()
