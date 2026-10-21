#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Formats Python files with autoflake, isort, and black.

Writes a JSON object to the --output file that maps each path whose contents
changed to its formatted contents.

The tools make conflicting style changes, so each file must go through them in
order. Running the tools once per file is slow when checking thousands of files,
so instead this script copies the files into a temporary directory and runs
each tool once over all the copies.
"""

import argparse
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from importlib import metadata
from pathlib import Path


def _run(cmd: list[str], copy_root: Path) -> None:
    # Run in copy_root with relative paths so that error messages refer to the
    # original paths.
    res = subprocess.run(
        cmd,
        cwd=copy_root,
        # Black caches results by absolute path, so give it a throwaway cache
        # rather than filling the user's cache with temporary paths.
        env={**os.environ, "BLACK_CACHE_DIR": str(copy_root / ".black-cache")},
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if res.returncode != 0:
        sys.stderr.write(res.stdout)
        sys.exit(res.returncode)


def _autoflake(
    args: argparse.Namespace, files: Sequence[str], copy_root: Path
) -> None:
    jobs = min(len(files), os.cpu_count() or 1)
    _run(
        [
            args.python,
            args.autoflake,
            f"--jobs={jobs}",
            "--in-place",
            "--remove-unused-variables",
            "--remove-all-unused-imports",
            "--remove-duplicate-keys",
            "--ignore-init-module-imports",
            *files,
        ],
        copy_root,
    )


def _isort(
    args: argparse.Namespace, files: Sequence[str], copy_root: Path
) -> None:
    import isort

    cwd = Path.cwd().resolve()
    default_config: isort.Config | None = None
    configs: dict[str, isort.Config] = {}
    for f in files:
        # isort's config and its classification of first-party imports depend
        # on where a file lives, so sort each copy as if it were the original
        # file, like `isort --filename <f> -` does.
        directory = os.path.dirname(f) or "."
        if directory not in configs:
            # Overriding isort's default skip list keeps it from skipping files
            # under directories with common names like "build".
            config = isort.Config(settings_path=directory, skip=[".venvs"])
            # isort stops searching upward at a `.git` directory, so in a
            # subrepo without its own config (like //vendor/google) it finds
            # nothing in a standard checkout, whereas in a git worktree `.git`
            # is a regular file and isort walks past the repo root. Fall back
            # to --pyproject-toml whenever no pyproject.toml is found within
            # the repo.
            has_repo_config = any(
                s.get("source", "").endswith("pyproject.toml")
                for s in config.sources
            ) and Path(config.directory).resolve().is_relative_to(cwd)
            if not has_repo_config:
                if default_config is None:
                    default_config = isort.Config(
                        settings_file=args.pyproject_toml,
                        directory=str(cwd),
                        skip=[".venvs"],
                    )
                config = default_config
            configs[directory] = config
        copy = copy_root / f
        try:
            with open(copy, encoding="utf-8", newline="") as file:
                original = file.read()
            # autoflake strips byte order marks, and isort treats them as code.
            content = original.removeprefix("\ufeff")
            output = io.StringIO()
            try:
                isort.api.sort_stream(
                    io.StringIO(content),
                    output,
                    config=configs[directory],
                    file_path=Path(f),
                    # Sort imports above any `isort: skip_file` comment, like
                    # the CLI.
                    raise_on_skip=False,
                )
            except isort.exceptions.FileSkipped:
                # The file matches the config's skip settings.
                continue
        except Exception as e:
            raise RuntimeError(f"isort failed on {f}") from e
        if output.getvalue() != original:
            with open(copy, "w", encoding="utf-8", newline="") as file:
                file.write(output.getvalue())


def _black(
    args: argparse.Namespace, files: Sequence[str], copy_root: Path
) -> None:
    _run(
        [
            args.black,
            "--config",
            args.pyproject_toml,
            "--quiet",
            *files,
        ],
        copy_root,
    )


def _relative_path(path: str) -> str:
    # Absolute paths or paths containing ".." would escape the temporary copy
    # directory, so the tools would modify the original files.
    if os.path.isabs(path) or ".." in Path(path).parts:
        raise argparse.ArgumentTypeError(f"must be a relative path: {path}")
    return path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", required=True, type=os.path.abspath)
    parser.add_argument("--autoflake", required=True, type=os.path.abspath)
    parser.add_argument("--isort", required=True, type=os.path.abspath)
    parser.add_argument("--black", required=True, type=os.path.abspath)
    parser.add_argument(
        "--pyproject-toml",
        required=True,
        type=os.path.abspath,
        help="default pyproject.toml config file",
    )
    parser.add_argument("--output", required=True, help="JSON output file")
    parser.add_argument("files", nargs="+", type=_relative_path)
    args = parser.parse_args()

    # Run isort from source like third_party/pylibs/isort/main.py.
    # TODO(b/353376647): Remove the metadata workaround along with the one in
    # third_party/pylibs/isort/main.py.
    sys.path.append(str(Path(args.isort).parent / "src"))
    try:
        metadata.version("isort")
    except metadata.PackageNotFoundError:
        metadata.version = lambda _: "unknown"  # type: ignore[assignment]

    with tempfile.TemporaryDirectory() as tmp_dir:
        copy_root = Path(tmp_dir)
        for f in args.files:
            (copy_root / f).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(f, copy_root / f)

        _autoflake(args, args.files, copy_root)
        _isort(args, args.files, copy_root)
        _black(args, args.files, copy_root)

        formatted = {}
        for f in args.files:
            content = (copy_root / f).read_bytes()
            if content != Path(f).read_bytes():
                formatted[f] = content.decode("utf-8")

    with open(args.output, "w", encoding="utf-8") as output:
        json.dump(formatted, output, ensure_ascii=False)


if __name__ == "__main__":
    main()
