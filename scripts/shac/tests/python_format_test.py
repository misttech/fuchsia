#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

_PYPROJECT = """[tool.isort]
profile = "black"
line_length = 80
"""

# Exercises all three tools: autoflake removes the unused `os` import, isort
# sorts and wraps the imports, and black reformats `1+2`.
_DIRTY = """import sys
import json
import os
from very_long_module_name_that_definitely_forces_isort_to_wrap_this_line import member_c, member_a, member_b

def foo():
    print(sys.argv)
    print(json.dumps({}))
    print(member_a, member_b, member_c)
    return 1+2
"""

_FORMATTED = """import json
import sys

from very_long_module_name_that_definitely_forces_isort_to_wrap_this_line import (
    member_a,
    member_b,
    member_c,
)


def foo():
    print(sys.argv)
    print(json.dumps({}))
    print(member_a, member_b, member_c)
    return 1 + 2
"""


class TestPythonFormat(unittest.TestCase):
    def __init__(
        self,
        python_bin: Path,
        autoflake: Path,
        isort: Path,
        black: Path,
        script_path: Path,
        methodName: str = "runTest",
    ):
        super().__init__(methodName)
        self.python_bin = python_bin
        self.autoflake = autoflake
        self.isort = isort
        self.black = black
        self.script_path = script_path

    def setUp(self) -> None:
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (self.root / "pyproject.toml").write_text(_PYPROJECT)

    def _write(self, path: str, content: str) -> None:
        (self.root / path).parent.mkdir(parents=True, exist_ok=True)
        (self.root / path).write_text(content)

    def _run(
        self, *files: str, cwd: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        output = self.root / "output.json"
        output.unlink(missing_ok=True)
        res = subprocess.run(
            [
                self.python_bin,
                self.script_path.absolute(),
                "--python",
                self.python_bin,
                "--autoflake",
                self.autoflake,
                "--isort",
                self.isort,
                "--black",
                self.black,
                "--pyproject-toml",
                self.root / "pyproject.toml",
                "--output",
                output,
                *files,
            ],
            cwd=cwd or self.root,
            capture_output=True,
            text=True,
        )
        if res.returncode == 0:
            res.stdout = output.read_text()
        return res

    def _format(self, *files: str, cwd: Path | None = None) -> dict[str, str]:
        res = self._run(*files, cwd=cwd)
        self.assertEqual(res.returncode, 0, res.stderr)
        result: dict[str, str] = json.loads(res.stdout)
        return result

    def test_format(self) -> None:
        # Files in a "build" directory check that isort doesn't skip
        # directories that its default skip list includes.
        self._write("build/clean.py", _FORMATTED)
        self._write("build/dirty.py", _DIRTY)
        self._write("dirty.py", _DIRTY)
        # Only black changes this file.
        self._write("black_only.py", "x = [1,2]\n")

        self.assertEqual(
            self._format(
                "build/clean.py", "build/dirty.py", "dirty.py", "black_only.py"
            ),
            {
                "build/dirty.py": _FORMATTED,
                "dirty.py": _FORMATTED,
                "black_only.py": "x = [1, 2]\n",
            },
        )
        # The original files must not be modified.
        self.assertEqual((self.root / "dirty.py").read_text(), _DIRTY)

    def test_all_clean(self) -> None:
        self._write("clean.py", _FORMATTED)
        self.assertEqual(self._format("clean.py"), {})

    def test_nested_isort_config(self) -> None:
        # isort must use the config closest to each original file, and treat
        # modules next to that config as first-party.
        self._write(
            "pyproject.toml",
            '[tool.isort]\nprofile = "black"\ndefault_section = "FIRSTPARTY"\n',
        )
        self._write(
            "sub/pyproject.toml",
            '[tool.isort]\nprofile = "black"\ndefault_section = "THIRDPARTY"\n',
        )
        self._write("sub/local_module.py", "")
        self._write(
            "sub/main.py",
            "import local_module\nimport requests\n\n"
            "print(local_module, requests)\n",
        )
        want = (
            "import requests\n\nimport local_module\n\n"
            "print(local_module, requests)\n"
        )
        self.assertEqual(self._format("sub/main.py"), {"sub/main.py": want})
        # Also check a top-level file (`os.path.dirname(f) == ""`) when running
        # in `sub/`, ensuring `./pyproject.toml` takes precedence over
        # `--pyproject-toml`.
        self.assertEqual(
            self._format("main.py", cwd=self.root / "sub"), {"main.py": want}
        )

    def test_subrepo_uses_default_pyproject_toml(self) -> None:
        # A short multi-line import with a trailing comma is preserved when
        # isort uses `profile = "black"`, but collapsed onto one line if isort
        # falls back to its built-in defaults (`include_trailing_comma = False`).
        # Also verify that first-party detection is rooted at the subrepo
        # (`local_module` in `sub/vendor` is separated from third-party `foo`).
        self._write("sub/vendor/local_module.py", "")
        content = (
            "from foo import (\n    bar,\n)\n\n"
            "import local_module\n\n"
            "print(bar, local_module)\n"
        )
        self._write("sub/vendor/top.py", content)
        self._write("sub/vendor/pkg/nested.py", content)
        # Put a conflicting config in the parent directory to verify that in a
        # git worktree (where `.git` is a regular file), isort does not walk
        # past the subrepo root instead of using --pyproject-toml.
        self._write(
            "sub/pyproject.toml",
            "[tool.isort]\ninclude_trailing_comma = false\n",
        )

        subrepo = self.root / "sub" / "vendor"
        # Standard checkout: `.git` is a directory.
        (subrepo / ".git").mkdir()
        self.assertEqual(
            self._format("top.py", "pkg/nested.py", cwd=subrepo), {}
        )

        # Git worktree: `.git` is a regular file.
        (subrepo / ".git").rmdir()
        self._write("sub/vendor/.git", "gitdir: /elsewhere\n")
        self.assertEqual(
            self._format("top.py", "pkg/nested.py", cwd=subrepo), {}
        )

    def test_syntax_error(self) -> None:
        self._write("broken.py", "def foo(:\n")
        res = self._run("broken.py")
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("broken.py", res.stderr)

    def test_rejects_absolute_paths(self) -> None:
        self._write("dirty.py", _DIRTY)
        res = self._run(str(self.root / "dirty.py"))
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("must be a relative path", res.stderr)
        self.assertEqual((self.root / "dirty.py").read_text(), _DIRTY)


def main() -> None:
    parser = argparse.ArgumentParser(description="Run python format test.")
    parser.add_argument(
        "--python-bin", required=True, help="Path to python bin"
    )
    parser.add_argument(
        "--autoflake", required=True, help="Path to autoflake script"
    )
    parser.add_argument("--isort", required=True, help="Path to isort script")
    parser.add_argument("--black", required=True, help="Path to black binary")
    parser.add_argument(
        "--python-format-script",
        required=True,
        help="Path to python_format.py",
    )

    args = parser.parse_args()

    # Create custom suite to pass arguments to test instance
    suite = unittest.TestSuite()
    for method_name in unittest.TestLoader().getTestCaseNames(TestPythonFormat):
        suite.addTest(
            TestPythonFormat(
                python_bin=Path(args.python_bin).absolute(),
                autoflake=Path(args.autoflake).absolute(),
                isort=Path(args.isort).absolute(),
                black=Path(args.black).absolute(),
                script_path=Path(args.python_format_script),
                methodName=method_name,
            )
        )

    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    sys.exit(not result.wasSuccessful())


if __name__ == "__main__":
    main()
