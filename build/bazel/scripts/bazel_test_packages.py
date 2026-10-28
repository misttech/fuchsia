# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Exports Bazel test packages to paths owned by the Ninja build.

Bazel test packages (`fx_test()` targets) are built in the Bazel output base,
but Ninja-side consumers like the amber-files publish, `fx test` and testsharder
read them after the Bazel invocation that built them has finished. Paths in the
output base aren't stable across invocations:

  * Blobs from external repositories (e.g. `ld.so.1` from the sysroot, or the
    C++ runtime libraries) are reached through the `execroot/_main/external/`
    symlinks, which every Bazel invocation replants with only the repositories
    that it needs.
  * With `--remote_download_outputs=toplevel`, Bazel only guarantees that an
    output is on disk at the end of an invocation that requested it
    (https://fxbug.dev/576327980).

So right after the Bazel invocation that builds the test packages, this module
exports every blob into a content-addressed directory in the Ninja build
directory, and writes a copy of each package manifest pointing at those blobs.
Blobs built by Bazel are hardlinked, which keeps their contents alive even if
Bazel later deletes or replaces its own copy.

This can't be a Bazel rule whose outputs `bazel_action()` copies with
`directory_outputs`: the test packages reach Bazel as command line targets
(the `test_suite()`s in `extra_bazel_targets_file`), and a `test_suite()` has
no default outputs, so no Bazel target can depend on all of them.
"""

import argparse
import errno
import json
import os
import shutil
import sys
import typing as T
from pathlib import Path

from build.bazel.scripts import build_utils

# The directory, relative to the Ninja build directory, that holds all
# exported Bazel test packages. It's named after, and lives next to,
# PACKAGES_LIST, which lists the packages it contains.
EXPORT_DIR = "bazel_test_packages"

# The list of exported package manifests, relative to the Ninja build
# directory. Written by `bazel_tests_utils.py` at `fx gen` time, in the schema
# of all_package_manifests.list.
# LINT.IfChange(bazel_test_packages_list)
PACKAGES_LIST = "bazel_test_packages.list"
# LINT.ThenChange(//scripts/fxtest/python/main.py:bazel_test_packages_list)

_MANIFESTS_SUBDIR = "manifests"
_BLOBS_SUBDIR = "blobs"
# Records, for every exported package, the (mtime, size) of the manifest that
# Bazel wrote, so that incremental exports can skip packages that Bazel hasn't
# rebuilt since. Lives in EXPORT_DIR.
_STATE_FILE = "exported.json"


def export_path_for_manifest(execroot_path: str) -> str:
    """Returns where to export a package manifest.

    Args:
        execroot_path: The path of a package manifest built by Bazel, relative
            to the Bazel execroot.

    Returns:
        The path of the exported manifest, relative to the Ninja build
        directory.
    """
    return os.path.join(EXPORT_DIR, _MANIFESTS_SUBDIR, execroot_path)


def execroot_path_for_export(export_path: str) -> str:
    """The inverse of `export_path_for_manifest()`."""
    prefix = os.path.join(EXPORT_DIR, _MANIFESTS_SUBDIR) + os.sep
    if not export_path.startswith(prefix):
        raise ValueError(
            f"Not the path of an exported Bazel test package manifest: {export_path}"
        )
    return export_path.removeprefix(prefix)


class _Exporter:
    """Exports Bazel test packages into EXPORT_DIR.

    Usage: create one instance per export run, call `export()` once for each
    package to export, then call `prune()` once at the end with the export
    paths of *every* package that should remain exported, including any that
    weren't re-exported in this run (see `export_packages()`).

    `prune()` must come last because it deletes every exported blob and
    manifest that isn't reachable from the exported manifests it's given, as
    they currently exist on disk. Pruning before an `export()` would keep the
    blobs of the package's previous version around, and passing only the
    packages exported in this run would delete all the others.
    """

    def __init__(self, bazel_paths: build_utils.BazelPaths) -> None:
        self._execroot = bazel_paths.execroot
        self._ninja_build_dir = bazel_paths.ninja_build_dir
        self._output_base = bazel_paths.output_base
        self._blobs_dir = self._ninja_build_dir / EXPORT_DIR / _BLOBS_SUBDIR
        # Execroot-relative paths of the manifests exported so far, to avoid
        # exporting a subpackage shared by several packages more than once.
        self._exported: set[str] = set()

    def export(self, execroot_path: str) -> None:
        """Exports the package manifest at `execroot_path` and its subpackages."""
        if execroot_path in self._exported:
            return
        self._exported.add(execroot_path)

        manifest_path = self._execroot / execroot_path
        with open(manifest_path) as f:
            manifest = json.load(f)

        export_path = self._ninja_build_dir / export_path_for_manifest(
            execroot_path
        )
        export_dir = export_path.parent

        relative_to_file = manifest.get("blob_sources_relative") == "file"

        def resolve(path: str) -> Path:
            if relative_to_file:
                return Path(os.path.normpath(manifest_path.parent / path))
            # Bazel runs package-tool in the execroot, so working-dir-relative
            # paths are relative to it.
            return Path(os.path.normpath(self._execroot / path))

        for blob in manifest["blobs"]:
            exported_blob = self._blobs_dir / blob["merkle"]
            # Blobs are named by their Merkle root, so an existing file with the
            # expected size already has the right contents, even if Bazel's
            # copy has since disappeared (e.g. because its external repository
            # isn't planted in the execroot anymore).
            if not _has_size(exported_blob, blob["size"]):
                source = resolve(blob["source_path"])
                if not source.is_file():
                    raise FileNotFoundError(
                        f"Blob {blob['path']} of Bazel test package "
                        f"{manifest_path} is missing: {source}"
                    )
                self._export_blob(source, exported_blob)
            blob["source_path"] = os.path.relpath(exported_blob, export_dir)

        for subpackage in manifest.get("subpackages", []):
            subpackage_path = resolve(subpackage["manifest_path"])
            subpackage_execroot_path = os.path.relpath(
                subpackage_path, self._execroot
            )
            if subpackage_execroot_path.startswith(os.pardir + os.sep):
                raise ValueError(
                    f"Subpackage manifest {subpackage_path} of Bazel test "
                    f"package {manifest_path} is outside the Bazel execroot"
                )
            self.export(subpackage_execroot_path)
            subpackage["manifest_path"] = os.path.relpath(
                self._ninja_build_dir
                / export_path_for_manifest(subpackage_execroot_path),
                export_dir,
            )

        manifest["blob_sources_relative"] = "file"
        _write_if_changed(export_path, json.dumps(manifest, indent=2) + "\n")

    def _export_blob(self, source: Path, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_name(dest.name + ".tmp")
        tmp.unlink(missing_ok=True)

        # Hardlinking is only safe if nothing writes to the file in place,
        # since that would change the exported blob behind its Merkle root's
        # back. Bazel replaces its outputs rather than writing to them, so
        # outputs in the output base are hardlinked. Anything else (e.g. a
        # source file, which an editor may rewrite in place, or a prebuilt) is
        # copied.
        real_source = source.resolve()
        if real_source.is_relative_to(self._output_base.resolve()):
            try:
                os.link(real_source, tmp)
            except OSError as e:
                if e.errno != errno.EXDEV:
                    raise
                shutil.copyfile(real_source, tmp)
        else:
            shutil.copyfile(real_source, tmp)
        os.replace(tmp, dest)

    def prune(self, export_paths: T.Iterable[str]) -> set[Path]:
        """Deletes exported files that the listed packages don't refer to.

        Without this, every blob that was ever exported would stay on disk:
        once Bazel replaces its own copy, the exported hardlink is the only
        remaining reference to the old contents. Manifests of packages that
        are no longer listed are deleted too, since their blobs are gone.

        Returns:
            The exported manifests that were kept.
        """
        referenced_blobs: set[str] = set()
        referenced_manifests: set[Path] = set()
        pending = [self._ninja_build_dir / p for p in export_paths]
        while pending:
            path = pending.pop()
            if path in referenced_manifests or not path.is_file():
                continue
            referenced_manifests.add(path)
            with open(path) as f:
                manifest = json.load(f)
            referenced_blobs.update(b["merkle"] for b in manifest["blobs"])
            pending.extend(
                Path(os.path.normpath(path.parent / s["manifest_path"]))
                for s in manifest.get("subpackages", [])
            )

        if self._blobs_dir.is_dir():
            for blob in self._blobs_dir.iterdir():
                if blob.name not in referenced_blobs:
                    blob.unlink()
        manifests_dir = self._ninja_build_dir / EXPORT_DIR / _MANIFESTS_SUBDIR
        if manifests_dir.is_dir():
            for dirpath, _, filenames in os.walk(manifests_dir):
                for filename in filenames:
                    path = Path(dirpath) / filename
                    if path not in referenced_manifests:
                        path.unlink()
        return referenced_manifests


def export_packages(
    bazel_paths: build_utils.BazelPaths,
    export_paths: list[str],
    incremental: bool = False,
) -> None:
    """Exports Bazel test packages to the Ninja build directory.

    Args:
        bazel_paths: The paths of the Bazel workspace and the Ninja build dir.
        export_paths: Paths returned by `export_path_for_manifest()` for the
            packages to export, relative to the Ninja build directory.
        incremental: Only export packages that Bazel has rebuilt since they
            were last exported, and skip the ones it has never built. This is
            for builds of individual Bazel test targets (e.g. by `fx test`),
            which don't build every package, and after which the files of the
            packages they didn't build may be missing from the output base.

    Raises:
        FileNotFoundError: A package manifest (unless `incremental` is set)
            or one of its blobs doesn't exist.
    """
    exporter = _Exporter(bazel_paths)
    state_file = bazel_paths.ninja_build_dir / EXPORT_DIR / _STATE_FILE
    previous_state = _read_state(state_file) if incremental else {}
    state: dict[str, list[int]] = {}
    for export_path in export_paths:
        execroot_path = execroot_path_for_export(export_path)
        manifest = bazel_paths.execroot / execroot_path
        try:
            stat = manifest.stat()
        except FileNotFoundError:
            if incremental:
                # Keep what was exported before, if anything.
                if execroot_path in previous_state:
                    state[execroot_path] = previous_state[execroot_path]
                continue
            raise
        manifest_state = [stat.st_mtime_ns, stat.st_size]
        state[execroot_path] = manifest_state
        if (
            incremental
            and previous_state.get(execroot_path) == manifest_state
            and (bazel_paths.ninja_build_dir / export_path).is_file()
        ):
            continue
        exporter.export(execroot_path)
    exporter.prune(export_paths)
    _write_if_changed(state_file, json.dumps(state, indent=2, sort_keys=True))


def _read_state(state_file: Path) -> dict[str, list[int]]:
    try:
        with open(state_file) as f:
            state: dict[str, list[int]] = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    return state


def read_packages_list(ninja_build_dir: Path) -> list[str]:
    """Reads the list of exported package manifests written at `fx gen` time."""
    with open(ninja_build_dir / PACKAGES_LIST) as f:
        manifests: list[str] = json.load(f)["content"]["manifests"]
    return manifests


def _has_size(path: Path, size: int) -> bool:
    try:
        return path.stat().st_size == size
    except FileNotFoundError:
        return False


def _write_if_changed(path: Path, content: str) -> None:
    # Avoid bumping the mtime of unchanged manifests, which would make Ninja
    # rerun everything that reads them.
    if path.is_file() and path.read_text() == content:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(content)
    os.replace(tmp, path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--ninja-build-dir",
        type=Path,
        required=True,
        help="The Ninja build directory.",
    )
    parser.add_argument(
        "--incremental",
        action="store_true",
        help="Only export packages that Bazel has built since they were last "
        "exported.",
    )
    args = parser.parse_args()

    bazel_paths = build_utils.BazelPaths.new(build_dir=args.ninja_build_dir)
    try:
        export_packages(
            bazel_paths,
            read_packages_list(bazel_paths.ninja_build_dir),
            incremental=args.incremental,
        )
    except (OSError, KeyError, ValueError) as e:
        print(
            f"ERROR: Failed to export Bazel test packages: {e}", file=sys.stderr
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
