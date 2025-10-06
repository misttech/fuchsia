#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Verify the runtime dependencies of a prebuilt SDK library (static or shared).
See //build/cpp/verify_runtime_deps.gni for details. This takes two input files:

- A JSON file that lists the runtime dependencies of the library, each
  one of them through a schema described in //build/cpp/verify_runtime_deps.gni

- An SDK manifest file, another JSON file that describes the SDK atom for
  the target as well as _all_ its transitive dependencies. See sdk_atom()
  template for more details.

On success, a stamp file is written. On failure, error messages are printed
to stderr and the script returns with an error status.
"""

import argparse
import collections
import json
import os
import sys
import typing as T

# The following runtime libraries are provided directly by the SDK sysroot,
# and not as SDK atoms.
_SYSROOT_LIBS = ["libc.so", "libzircon.so"]

# Information about an atom.
AtomInfo: T.TypeAlias = dict[str, T.Any]

# The contents of a JSON file.
JsonFileContent: T.TypeAlias = dict[str, T.Any]


def parse_sdk_manifest(
    manifest: JsonFileContent,
) -> tuple[str, dict[str, AtomInfo]]:
    """Parse SDK manifest file and extract atom id and dependencies.

    Args:
      manifest: A directionary representation of the JSON SDK manifest.

    Returns:
      an (sdk_id, deps) tuple, where 'sdk_id' is a string identifying the
      atom (e.g. 'sdk://pkg/async'), and 'deps' is a dictionary mapping
      the sdk ids of all transitive dependencies to the corresponding
      manifest JSON object.
    """
    atom_id = manifest["ids"][0]

    def find_atom(id: str) -> AtomInfo:
        return next(a for a in manifest["atoms"] if a["id"] == id)

    atom = find_atom(atom_id)
    # print(atom)
    deps = [find_atom(a) for a in atom["deps"]]
    deps += [atom]

    # Maps sdk_ids to the corresponding entry
    deps_map = {dep["id"]: dep for dep in deps}

    return atom_id, deps_map


_NON_SDK_DEPS_ERROR_HEADER = r"""## Non-SDK dependencies required at runtime:
"""

_NON_SDK_DEPS_ERROR_FOOTER = r"""
HINT: These should be defined by an sdk_shared_library() call, or by a
zx_library() one that sets 'sdk = "shared"'.
"""

_BAD_SDK_DEPS_ERROR_HEADER = r"""## Non prebuilt libraries required at runtime:
"""

_BAD_SDK_DEPS_ERROR_FOOTER = _NON_SDK_DEPS_ERROR_FOOTER

_MISSING_SDK_DEPS_ERROR_HEADER = r"""## No dependency generates SDK runtime requirement:
"""

_MISSING_SDK_DEPS_ERROR_FOOTER = r"""
HINT: Add the missing atom(s)'s targets to the 'runtime_deps' list.
"""


class DependencyErrors(object):
    """Models list of errors found during verification."""

    def __init__(self) -> None:
        self._errors: list[tuple[dict[str, str], str]] = []

    def has_error(self) -> bool:
        """Return True iff this instance contains errors."""
        return bool(self._errors)

    def add_non_sdk_dependency(self, entry: dict[str, str]) -> None:
        """Add an entry for a non-SDK dependency.

        This happens when an SDK atom depends on a shared_library() instance
        directly, instead of an skd_shared_library() one.
        """
        self._errors.append((entry, "non_sdk_deps"))

    def add_bad_sdk_dependency(self, entry: dict[str, str]) -> None:
        """Add an entry for a non-prebuilt shared library."""
        self._errors.append((entry, "bad_sdk_deps"))

    def add_missing_sdk_dependency(self, entry: dict[str, str]) -> None:
        """Add an entry for a non-SDK shared library dependency."""
        self._errors.append((entry, "missing_sdk_deps"))

    def __str__(self) -> str:
        result = ""
        # Split errors by categories
        errors = collections.defaultdict(list)
        for entry, category in self._errors:
            errors[category].append(entry)

        for category, entries in errors.items():
            if category == "non_sdk_deps":
                result += _NON_SDK_DEPS_ERROR_HEADER
                for entry in entries:
                    result += "- `%s` generated_by `%s`\n" % (
                        entry["source"],
                        entry["label"],
                    )
                result += _NON_SDK_DEPS_ERROR_FOOTER

            elif category == "bad_sdk_deps":
                result += _BAD_SDK_DEPS_ERROR_HEADER
                for entry in entries:
                    result += "- IDK atom `%s` generated_by `%s`\n" % (
                        entry["sdk_id"],
                        entry["label"],
                    )
                result += _BAD_SDK_DEPS_ERROR_FOOTER

            elif category == "missing_sdk_deps":
                result += _MISSING_SDK_DEPS_ERROR_HEADER
                for entry in entries:
                    result += "- IDK atom `%s` generated_by `%s`\n" % (
                        entry["sdk_id"],
                        entry["label"],
                    )
                result += _MISSING_SDK_DEPS_ERROR_FOOTER

            else:
                assert False, "Unknown category: %s" % category

        return result


def check_for_missing_runtime_deps(
    runtime_files: T.Sequence[dict[str, str]], atom_deps: dict[str, AtomInfo]
) -> DependencyErrors:
    """Verifies the runtime dependencies of a prebuilt IDK library (static or shared).

    Verifies the atoms corresponding to the list of runtime requirements in
    `runtime_files` are all included in `atom_deps`.

    This ensures that atoms containing all runtime files are included in the
    atom's deps so that IDK consumers will know to package them when using
    the atom. This also verifies those atoms will be included in the IDK.

    Args:
      runtime_files: A JSON list of dictionaries, each representing a runtime
        dependency. Each dictionary must follow the schema described in
        //build/cpp/verify_runtime_deps.gni and contain either an 'sdk_id' or a
        'source' key.
      atom_deps: A dictionary mapping the atom IDs of the atom and its
        dependencies to their corresponding atom information. (The
        implementation does not care whether the dependencies are only direct or
        include all its transitive dependencies, but the results may vary
        depending on this.)

    Returns:
      A DependencyErrors instance containing any errors found during
      verification.
    """
    errors = DependencyErrors()

    for entry in runtime_files:
        sdk_id = entry.get("sdk_id")
        source = entry.get("source")

        assert bool(sdk_id) != bool(source), (
            'Exactly one of "sdk_id" or "source" must be defined in entry: %s'
            % entry
        )

        if sdk_id:
            # This runtime dependency is an IDK library. Verify that the library
            # is in the atom's deps.
            assert not source, "Only one of `sdk_id` or `source` should be set."
            dep = atom_deps.get(sdk_id)
            if not dep:
                errors.add_missing_sdk_dependency(entry)
            elif "type" in dep:
                # The atom info is from the old .sdk build manifest.
                if dep["type"] != "cc_prebuilt_library":
                    errors.add_bad_sdk_dependency(entry)
            # Else the atom info is from prebuild info.
            elif dep["atom_type"] != "cc_prebuilt_library":
                errors.add_bad_sdk_dependency(entry)
        elif source:
            # A non-IDK library.

            # Ignore sysroot libs
            if os.path.basename(source) in _SYSROOT_LIBS:
                # TODO(https://fxbug.dev/447151364): Determine whether this
                # unused logic is necessary.
                assert False
                continue

            # This runtime dependency is *not* an IDK library. This is an error.
            errors.add_non_sdk_dependency(entry)
        else:
            assert False, "Runtime entry is missing 'sdk_id' or 'source'."

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--sdk-runtime-deps",
        help="Path to the list of runtime deps.",
        required=True,
    )
    parser.add_argument(
        "--sdk-manifest",
        help="Path to the target's SDK manifest file.",
        required=True,
    )
    parser.add_argument(
        "--stamp-file", help="Path to the output stamp file.", required=True
    )
    args = parser.parse_args()

    with open(args.sdk_runtime_deps, "r") as runtime_deps_file:
        runtime_files = json.load(runtime_deps_file)

    # Read the list of package dependencies for the library's SDK incarnation.
    with open(args.sdk_manifest, "r") as manifest_file:
        manifest = json.load(manifest_file)

    atom_id, deps = parse_sdk_manifest(manifest)

    # Find atom label with `_sdk_manifest($toolchain_suffix)` removed
    atom_label, _, _ = deps[atom_id]["gn-label"].rpartition("_sdk_manifest(")

    # Check whether all runtime files are available for packaging.
    errors = check_for_missing_runtime_deps(runtime_files, deps)
    if errors.has_error():
        print(
            r"""
ERROR: When verifying runtime dependencies for the IDK atom: `%s` generated_by `%s`

%s"""
            % (atom_id, atom_label, errors),
            file=sys.stderr,
        )
        return 1

    with open(args.stamp_file, "w") as stamp:
        stamp.write("Success!")

    return 0


if __name__ == "__main__":
    sys.exit(main())
