#!/usr/bin/env fuchsia-vendored-python
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Determines the files that needed to be rebuilt by fx ninja.

The primary use of this script is to find which build targets were changed
by a particular CL or patch for use by the clang static analyzer.

The input will most likely be the compile_commands.json generated by GN
which is normally out/<build-dir>/compile_commands.json

The output is a json file containing the translation units (TUs) taken
from the input compile_commands.json of which the build targets were
modified by a fx ninja call.

The expectated workflow is:
  1. Check out the base fuchsia commit against which to apply the patch.
  1. Run `fx build`.
  1. Apply the patch.
  1. Run this script.
  1. Run analyze-build against the output of this script.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time

import helper as color


def main(input_args: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Runs `fx ninja build` on a set of targets for compile translation units "
        "and determines which files were modified since that change."
    )
    parser.add_argument(
        "-i",
        "--input",
        required=True,
        help="The path of a compile_commands.json generated by GN.",
    )
    parser.add_argument(
        "-o",
        "--output",
        required=True,
        help="The path to write the output file of an array of paths for modified files in JSON format.",
    )
    parser.add_argument(
        "-n",
        "--ninja",
        required=True,
        help="Path to the prebuilt ninja compiler.",
    )
    args = parser.parse_args(input_args)

    with open(args.input) as compile_commands:
        res = ninja_build_tu(json.load(compile_commands), args.ninja)

        if not res:
            print(color.white("Did not find any changed files."))

        with open(args.output, "w") as out:
            json.dump(res, out, indent=2)

        print(color.white("Wrote to output file"), color.yellow(args.output))
    return 0


def ninja_build_tu(
    compdb: list[dict[str, str]], ninja_path: str
) -> list[dict[str, str]]:
    """Find build targets that were modified since last build.

    Attemps to build all build targets specified, returning the subset
    of those that require a rebuild since the last build. Leverages `fx
    ninja` in order to determine which build targets needed rebuilding.

    Args:
        compdb: A json-style list containing dictionary-like entries
                for each translation unit.
        ninja_path: Path to the ninja executable.

    Returns:
        A subset of the input compdb, only keeping the translation units
        that correspond to build targets that had been modified by the
        `ninja` call.
    """
    files: dict[str, list[str]] = {}
    tus: dict[str, list[dict[str, str]]] = {}
    # Find all TU targets
    for tu in compdb:
        directory = tu["directory"]
        command = tu["command"]
        command_entries = command.split()

        # Gather all targets in this TU
        for i in range(0, len(command_entries) - 1):
            if command_entries[i] != "-o":
                continue

            target = command_entries[i + 1]

            if directory not in files:
                files[directory] = []

            files[directory].append(target)

            # Store TU for reverse lookup later
            target_path = os.path.join(directory, target)
            if target_path not in tus:
                tus[target_path] = []
            tus[target_path].append(tu)

    modified_files = [
        process_tu_dir_xargs(directory, files[directory], ninja_path)
        for directory in files
    ]

    # Find all TUs that match modified files
    out_tus = []
    for _files in modified_files:
        for target in _files:
            if target in tus:
                out_tus.extend(tus[target])
            else:
                sys.exit(
                    "Expected %s to exist in reverse lookup dict." % (target)
                )
    return out_tus


def process_tu_dir_xargs(
    directory: str, build_targets: list[str], ninja_path: str
) -> list[str]:
    """Find build targets that were rebuilt.

    Takes an input directory (corresponding to the -C option in fx ninja) and
    a list of build targets. Based on system modified time, returns the list
    of build targets that have a new modified time.

    Args:
        directory: The string path name correpsonding to the build_targets.
        build_targets: A list of build targets to attempt to build.
        ninja_path: Path to the ninja executable.

    Returns:
        The subset of build_targets which were modified by `ninja`.
    """
    # Write targets to a temp file
    with tempfile.NamedTemporaryFile(mode="w") as fp:
        for f in build_targets:
            fp.write("%s\n" % (f))
        fp.flush()
        filename = os.path.realpath(fp.name)

        # The argument list may exceed the input buffer, so we use xargs to
        # call fx ninja multiple times, each with a subset of the arguments
        # commands = ['xargs', '-a', filename, 'fx', 'ninja', '-C', directory]
        commands = ["xargs", "-a", filename, ninja_path, "-C", directory]

        # Record the time before running fx ninja
        before = time.time()
        print(color.white("Running command"), color.yellow(" ".join(commands)))

        try:
            subprocess.run(commands, capture_output=True, check=True)
        except subprocess.CalledProcessError as err:
            print(
                color.red("Failed with exit code"),
                color.white(str(err.returncode)),
            )
            print(color.red(err.stderr))
            sys.exit("Error in fx ninja.")

    # Find all files that have a newer modified time than `before`
    modified_targets = []
    for file in build_targets:
        file_path = os.path.join(directory, file)
        if not os.path.isfile(file_path):
            continue
        mtime = os.path.getmtime(file_path)
        if mtime > before:
            modified_targets.append(file_path)

    return modified_targets


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
