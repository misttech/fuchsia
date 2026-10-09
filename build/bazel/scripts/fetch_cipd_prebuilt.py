#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fetch an on-demand CIPD prebuilt package via `jiri fetch-package` and materialize it in bazel-out/."""

import argparse
import os
import shutil
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

_SCRIPT_DIR = Path(__file__).resolve().parent
_DEFAULT_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent.parent


def _link_or_copy_file(src: str | Path, dst: str | Path) -> None:
    """Materialize a file from `src` to `dst` using a hardlink, falling back to copy."""
    src_path = Path(src).resolve()
    dst_path = Path(dst)
    dst_path.parent.mkdir(parents=True, exist_ok=True)
    if dst_path.exists() or dst_path.is_symlink():
        dst_path.unlink()
    try:
        os.link(src_path, dst_path)
    except OSError:
        shutil.copy2(src_path, dst_path)


def _materialize_directory(src_dir: Path, dst_dir: Path) -> None:
    """Materialize a directory tree from `src_dir` to `dst_dir` using hardlinks or copies."""
    src_dir_resolved = src_dir.resolve()
    if dst_dir.exists() or dst_dir.is_symlink():
        if dst_dir.is_dir() and not dst_dir.is_symlink():
            shutil.rmtree(dst_dir)
        else:
            dst_dir.unlink()
    dst_dir.mkdir(parents=True, exist_ok=True)
    shutil.copytree(
        src_dir_resolved,
        dst_dir,
        symlinks=True,
        copy_function=_link_or_copy_file,
        dirs_exist_ok=True,
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--path",
        required=True,
        help="Prebuilt path relative to Fuchsia root (e.g. 'prebuilt/third_party/...').",
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=Path,
        default=None,
        help="Path to Fuchsia root directory (defaults to FUCHSIA_DIR env var or inferred from script path).",
    )
    output_group = parser.add_mutually_exclusive_group(required=True)
    output_group.add_argument(
        "--output-file",
        type=Path,
        help="Destination path for a single file output.",
    )
    output_group.add_argument(
        "--output-dir",
        type=Path,
        help="Destination path for a directory output.",
    )
    args = parser.parse_args(argv)

    if args.fuchsia_dir is not None:
        fuchsia_dir = args.fuchsia_dir.resolve()
    elif "FUCHSIA_DIR" in os.environ:
        fuchsia_dir = Path(os.environ["FUCHSIA_DIR"]).resolve()
    else:
        fuchsia_dir = _DEFAULT_FUCHSIA_DIR

    if not (fuchsia_dir / ".jiri_root").is_dir():
        print(
            f"ERROR: Fuchsia root directory '{fuchsia_dir}' does not contain a '.jiri_root' directory.",
            file=sys.stderr,
        )
        return 1

    norm_path = args.path.removeprefix("//").strip("/")
    if not norm_path.startswith("prebuilt/"):
        print(
            f"ERROR: Prebuilt path must start with 'prebuilt/', got: {args.path}",
            file=sys.stderr,
        )
        return 1

    jiri_bin = fuchsia_dir / ".jiri_root" / "bin" / "jiri"
    if not jiri_bin.is_file():
        print(
            f"ERROR: jiri binary not found at {jiri_bin}",
            file=sys.stderr,
        )
        return 1

    cmd = [str(jiri_bin), "fetch-package", norm_path]
    result = subprocess.run(
        cmd,
        cwd=fuchsia_dir,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(
            f"ERROR: Command {cmd} failed with exit code {result.returncode}:\n"
            f"stdout:\n{result.stdout}\n"
            f"stderr:\n{result.stderr}",
            file=sys.stderr,
        )
        return result.returncode

    src_path = fuchsia_dir / norm_path
    if not src_path.exists():
        print(
            f"ERROR: Expected prebuilt path does not exist after fetching: {src_path}",
            file=sys.stderr,
        )
        return 1

    if args.output_file is not None:
        if not src_path.is_file():
            print(
                f"ERROR: Expected a file at {src_path}, but found a directory. "
                "Use `cipd_prebuilt_directory` instead of `cipd_prebuilt_file`.",
                file=sys.stderr,
            )
            return 1
        _link_or_copy_file(src_path, args.output_file)
    elif args.output_dir is not None:
        if not src_path.is_dir():
            print(
                f"ERROR: Expected a directory at {src_path}, but found a file. "
                "Use `cipd_prebuilt_file` instead of `cipd_prebuilt_directory`.",
                file=sys.stderr,
            )
            return 1
        _materialize_directory(src_path, args.output_dir)

    return 0


if __name__ == "__main__":
    sys.exit(main())
