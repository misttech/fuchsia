#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generate a host test wrapper and associated runtime directory and runtime deps list."""

import argparse
import json
import os
import shlex
import shutil
import sys
from pathlib import Path

sys.path.append(str(Path(__file__).parent / "../scripts"))
import build_utils
import runfiles_utils


def find_ninja_build_dir() -> Path:
    """Find the Ninja build directory.

    This only works if this script is invoked locally from the real Bazel execroot.
    It works by walking up the directory tree from the current working directory
    until it finds a directory containing a regenerator_outputs/ directory.

    Returns:
        The absolute path to the Ninja build directory.

    Raises:
        FileNotFoundError: If the Ninja build directory is not found.
    """
    start_path = Path.cwd()
    cur_path = start_path
    while cur_path != cur_path.parent:
        if (cur_path / "regenerator_outputs").is_dir():
            return cur_path.resolve()
        cur_path = cur_path.parent
    raise FileNotFoundError(
        f"Ninja build directory not found from: {start_path}"
    )


def remove_bazel_out_prefix(bazel_path: str) -> str:
    """Remove the bazel-out/<config_dir>/bin/ prefix from a Bazel path."""
    segments = bazel_path.split("/")
    assert (
        len(segments) > 3
        and segments[0] == "bazel-out"
        and segments[2] == "bin"
    ), f"Invalid bazel path: {bazel_path}"
    return "/".join(segments[3:])


def parse_data_runfile_path(
    runfile_path: str, fuchsia_dir: Path, bazel_execroot: Path
) -> tuple[str, Path]:
    """Parse a data runfile path into a canonical repo name and a short path.

    Args:
        runfile_path: The path to the data runfile.
        fuchsia_dir: Path to the Fuchsia source directory.
        bazel_execroot: Path to the Bazel execroot.

    Returns:
        A tuple of (rlocation, artifact_path), where rlocation is a string used both as
        the key and target in output_manifest_entries, and artifact_path is a Path value
        pointing to the actual file.
    """
    if runfile_path.startswith("bazel-out/"):
        # An artifact, the path is relative to the bazel execroot.
        rlocation = "_main/" + remove_bazel_out_prefix(runfile_path)
        artifact_path = bazel_execroot / runfile_path
    elif runfile_path.startswith("external/"):
        # An artifact path that belongs to an external repository.
        rlocation = runfile_path.removeprefix("external/")
        artifact_path = bazel_execroot / runfile_path
    else:
        # A source file, the path is relative to the workspace, which itself
        # symlinks the content of the Fuchsia source directory.
        rlocation = f"_main/{runfile_path}"
        artifact_path = fuchsia_dir / runfile_path

    return rlocation, artifact_path


def generate_test_wrapper(
    entry_point: Path,
    entry_runfiles_manifest: Path,
    test_label: str,
    output_launcher: Path,
    output_runtime_dir: Path,
    output_test_runtime_deps_json: Path,
    data_runfiles: list[str],
    test_args: list[str],
    bazel_execroot: Path,
) -> int:
    """Generate a Bazel host test wrapper script and related files.

    This function generates three files of interest:

    - A shell script used to invoke the actual test in the right work directory,
      and with hard-coded test arguments.

    - A directory to hold all runtime needed by the shell script. This includes the
      actual test binary, and all of its runfiles. Note that the runfiles manifest has
      been adjusted to only use paths relative to the runtime directory itself.

    - A JSON file containing the list of all files in the runtime directory, with paths
      relative to the Ninja build directory. This will be referenced by the tests.json
      entries for the test.

    Args:
        entry_point: The path to the entry point of the actual test, relative to the
            Bazel execroot. The name of that file is recorded as the test's name.
        entry_runfiles_manifest: The path to the runfiles manifest of the actual test,
            relative to the Bazel execroot.
        test_label: The label of the wrapper test, as it will appear in tests.json.
        output_launcher: The path to the output launcher script.
        output_runtime_dir: The path to the output runtime directory.
        output_test_runtime_deps_json: The path to the output test runtime deps JSON file.
        test_args: The arguments to pass to the test.
        bazel_execroot: The path to the Bazel execroot.
    """
    ninja_build_dir = find_ninja_build_dir()
    fuchsia_dir = build_utils.find_fuchsia_dir(from_path=ninja_build_dir)

    # Read //build/bazel/BAZEL_RUNFILES.md to understand the layout of the runfiles directory.

    # First, locate the input runfiles manifest, load it, and clean it up a little.
    input_manifest_path = bazel_execroot / entry_runfiles_manifest
    assert (
        input_manifest_path.exists()
    ), f"Missing Bazel runfiles manifest: {input_manifest_path}"
    input_manifest = runfiles_utils.RunfilesManifest.CreateFrom(
        input_manifest_path.read_text()
    )
    input_manifest.remove_legacy_external_runfiles(workspace_name="fuchsia")

    input_runfiles_dir = bazel_execroot / f"{entry_point}.runfiles"
    assert (
        input_runfiles_dir.exists()
    ), f"Missing Bazel runfiles directory: {input_runfiles_dir}"

    # Second, locate the _repo_mapping file from it. This should be an absolute path or
    # an execroot-relative one.
    repo_mapping_path = Path(input_manifest.lookup("_repo_mapping"))
    assert (
        repo_mapping_path
    ), f"Missing _repo_mapping entry from runfiles manifest at: {input_manifest_path}"
    if not repo_mapping_path.is_absolute():
        repo_mapping_path = bazel_execroot / repo_mapping_path
    assert (
        repo_mapping_path.exists()
    ), f"Missing Bazel repository mapping file: {repo_mapping_path}"

    def make_runtime_symlink(dest_path: Path, target_path: Path) -> None:
        """Create a symlink in the runtime directory.

        The target path must be resolved to an absolute path before creating the symlink.
        Without this, `bazel test --config=host <wrapper_test_label>` will fail because
        the test is run in a sandbox, which makes relative symlinks invalid.

        This does not affect `fx test`, which does not run the test in a sandbox.
        For infra builds, the content of the runtime directory is uploaded as a content-addressed
        directory of binary blobs after resolving the symlinks, so everything works when it
        is downloaded then run separately on test runner bots.
        """
        build_utils.force_raw_symlink(dest_path, target_path.resolve())

    if output_runtime_dir.exists():
        shutil.rmtree(output_runtime_dir)
    output_runtime_dir.mkdir(parents=True, exist_ok=True)

    # For every entry in the binary's runfiles manifest, create a corresponding symlink in the
    # output runfiles directory, but only use paths relative to runtime_dir.
    output_runfiles_dir = output_runtime_dir / f"{entry_point.name}.runfiles"
    output_runfiles_dir.mkdir(parents=True, exist_ok=True)

    output_manifest_entries: dict[str, str] = {}
    runtime_deps_paths: list[str] = []
    for source_path, target_path in input_manifest.as_dict().items():
        if not target_path:
            # This is an empty file in the input runfiles dir, create an empty
            # one in the output runfiles dir too. These are used for things like
            # Python __init__.py files.
            dest_path = output_runfiles_dir / source_path
            manifest_path = ""
            dest_path.parent.mkdir(parents=True, exist_ok=True)
            dest_path.write_text("")
        else:
            dest_path = output_runfiles_dir / source_path

            if not os.path.isabs(target_path):
                target_path = f"{bazel_execroot}/{target_path}"
            make_runtime_symlink(dest_path, Path(target_path))
            manifest_path = source_path

        output_manifest_entries[source_path] = manifest_path
        runtime_deps_paths.append(dest_path)

    # The data runfiles are not part of the binary's manifest and must be added to
    # the runtime_dir as symlinks, and to its manifest. The paths are "short" meaning
    # they are related to the bazel-bin/ directory, except those that belong in
    # external repositories, which begin with ../<canonical_repo_name>/
    for runfile_path in data_runfiles:
        rlocation, artifact_path = parse_data_runfile_path(
            runfile_path, fuchsia_dir, bazel_execroot
        )
        dest_path = output_runfiles_dir / rlocation
        make_runtime_symlink(dest_path, artifact_path)
        output_manifest_entries.setdefault(rlocation, rlocation)
        runtime_deps_paths.append(dest_path)

    # Create the MANIFEST file in the destination runfiles directory.
    # Unlike the input manifest, it cannot contain absollute paths, and all paths are
    # relative to the runtime_dir directory. This ensures that the corresponding test
    # can be run in isolation on a test sharder infra bot.
    exported_manifest = runfiles_utils.RunfilesManifest(
        {
            rlocation: f"{entry_point.name}.runfiles/{target_path}"
            for rlocation, target_path in output_manifest_entries.items()
        }
    )
    output_manifest_path = output_runfiles_dir / "MANIFEST"
    output_manifest_path.write_text(exported_manifest.generate_content())
    runtime_deps_paths.append(output_manifest_path)

    # Create a symlink in foo.runtime_dir for the runfiles manifest.
    output_manifest_symlink = (
        output_runtime_dir / f"{entry_point.name}.runfiles_manifest"
    )
    make_runtime_symlink(output_manifest_symlink, output_manifest_path)
    runtime_deps_paths.append(output_manifest_symlink)

    # Create a symlink in foo.runtime_dir for the entry point.
    output_entry_point = output_runtime_dir / entry_point.name
    make_runtime_symlink(output_entry_point, entry_point)
    runtime_deps_paths.append(output_entry_point)

    # Generate the launcher script
    output_launcher.parent.mkdir(parents=True, exist_ok=True)
    output_launcher.write_text(
        """#!/bin/bash
set -e
cd $(dirname "${{BASH_SOURCE[0]}}")/{runtime_dir_location}
echo "TEST PWD $(pwd)"
exec ./{entry_point_location} {test_args_prefix}"$@"
""".format(
            runtime_dir_location=os.path.relpath(
                output_runtime_dir, output_launcher.parent
            ),
            entry_point_location=shlex.quote(os.path.basename(entry_point)),
            test_args_prefix=" ".join([shlex.quote(arg) for arg in test_args])
            + " "
            if test_args
            else "",
        )
    )
    output_launcher.chmod(0o755)

    # Generate the test_runtime_deps.json file.
    output_test_runtime_deps_json.parent.mkdir(parents=True, exist_ok=True)
    output_test_runtime_deps_json.write_text(
        json.dumps(
            sorted(
                [
                    os.path.relpath(path, ninja_build_dir)
                    for path in runtime_deps_paths
                ]
            )
        )
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--entry-point",
        type=Path,
        required=True,
        help="The entry point to wrap.",
    )
    parser.add_argument(
        "--entry-runfiles-manifest",
        type=Path,
        required=True,
        help="The runfiles manifest of the entry point.",
    )
    parser.add_argument(
        "--test-label", type=str, required=True, help="The label of the test."
    )
    parser.add_argument(
        "--output-launcher",
        type=Path,
        required=True,
        help="The output launcher script.",
    )
    parser.add_argument(
        "--output-runtime-dir",
        type=Path,
        required=True,
        help="The output runtime directory.",
    )
    parser.add_argument(
        "--output-test-runtime-deps-json",
        type=Path,
        required=True,
        help="The output runtime_deps.json file.",
    )
    parser.add_argument(
        "--data-runfile",
        action="append",
        default=[],
        type=str,
        help="Data runfiles to include in the test's runfiles.",
    )
    parser.add_argument(
        "--test-arg",
        action="append",
        type=str,
        help="Extra arguments passed to the test entry point.",
    )
    parser.add_argument(
        "--bazel-execroot",
        type=Path,
        default=Path.cwd(),
        help="The Bazel execroot (default to current directory).",
    )
    args = parser.parse_args()

    return generate_test_wrapper(
        args.entry_point,
        args.entry_runfiles_manifest,
        args.test_label,
        args.output_launcher,
        args.output_runtime_dir,
        args.output_test_runtime_deps_json,
        args.data_runfile,
        args.test_arg,
        args.bazel_execroot,
    )


if __name__ == "__main__":
    sys.exit(main())
