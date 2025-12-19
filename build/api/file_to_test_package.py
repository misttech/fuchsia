# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import shlex
import typing as T
from pathlib import Path

if T.TYPE_CHECKING:
    from gn_ninja_outputs import OutputsDatabase


class FileToTestPackageFinder:
    """Finds test packages that cover a specific source file.

    This class encapsulates the logic for "fast path" lookups using
    build artifacts like rust-project.json and compile_commands.json,
    avoiding the slower full-graph analysis when possible.

    Args:
        build_dir: The root of the build directory.
        fuchsia_dir: The root of the Fuchsia source tree.
        outputs_database: An instance of OutputsDatabase for querying build outputs.
        log_func: A callable for logging messages.
    """

    def __init__(
        self,
        build_dir: Path,
        fuchsia_dir: Path,
        outputs_database: "OutputsDatabase",
        log_func: T.Callable[[str], None],
    ):
        self.build_dir = build_dir
        self.fuchsia_dir = fuchsia_dir
        self.outputs_database = outputs_database
        self.log_func = log_func

    def find_test_packages_fast(self, source_path: str) -> set[str]:
        """Finds test packages using fast-lookup heuristics.

        Args:
            source_path: The relative path to the source file (e.g., "src/lib.rs").

        Returns:
            A set of GN labels for test packages that likely cover this source file.
        """
        # Helper to get GN labels for the source file
        gn_labels = set()

        str_abs_source = os.path.realpath(self.fuchsia_dir / source_path)

        # 1. rust-project.json (for Rust)
        rust_project_path = self.build_dir / "rust-project.json"
        if rust_project_path.exists():
            try:
                # Use standard json load, assuming file is not huge or we have memory.
                # The original code loaded it all at once.
                with open(rust_project_path) as f:
                    rust_data = json.load(f)

                for crate in rust_data.get("crates", []):
                    root_module = crate.get("root_module", "")
                    if root_module == str_abs_source:
                        label = crate.get("label", "")
                        if label:
                            gn_labels.add(label)
                            break  # Found exact match

                    # Check include_dirs if not root
                    if not gn_labels:
                        source_opts = crate.get("source", {})
                        include_dirs = source_opts.get("include_dirs", [])
                        for d in include_dirs:
                            if str_abs_source.startswith(d):
                                label = crate.get("label", "")
                                if label:
                                    gn_labels.add(label)

                        if gn_labels:
                            break
            except (ValueError, OSError, json.JSONDecodeError) as e:
                self.log_func(f"rust-project.json search failed: {e}")

        # 2. compile_commands.json (for C++)
        if not gn_labels:
            cc_path = self.build_dir / "compile_commands.json"
            if cc_path.exists():
                with open(cc_path) as f:
                    cc_data = json.load(f)

                # Prepare relative path for matching
                # compile_commands.json usually uses paths relative to build_dir
                rel_source = os.path.relpath(str_abs_source, self.build_dir)

                for entry in cc_data:
                    file_path = entry.get("file", "")

                    if file_path == rel_source or file_path == str_abs_source:
                        match = True
                    elif not os.path.isabs(file_path):
                        # Resolve relative path from build_dir to see if it matches absolute source
                        candidate_abs = os.path.realpath(
                            self.build_dir / file_path
                        )
                        if candidate_abs == str_abs_source:
                            match = True
                        else:
                            match = False
                    else:
                        match = False

                    if match:
                        output = entry.get("output", "")
                        if not output:
                            # Try to extract from command
                            command = entry.get("command", "")
                            try:
                                args = shlex.split(command)
                                # Search backwards for the last -o flag
                                for i in range(len(args) - 1, -1, -1):
                                    arg = args[i]
                                    if arg == "-o":
                                        if i + 1 < len(args):
                                            output = args[i + 1]
                                            break
                                    elif arg.startswith("-o"):
                                        # Handle -oFILENAME
                                        output = arg[2:]
                                        break
                            except (ValueError, OSError) as e:
                                self.log_func(
                                    f"Error splitting command '{command}': {e}"
                                )

                        if output:
                            label = self.outputs_database.path_to_gn_label(
                                output
                            )
                            if label:
                                gn_labels.add(label)
                                break

        # If tests.json is missing, we cannot definitively find test packages.
        # However, if we found any related GN labels, return them as a fallback.
        tests_json_path = self.build_dir / "tests.json"
        if not tests_json_path.exists():
            if gn_labels:
                self.log_func(
                    f"WARNING: {tests_json_path} not found. Returning all found GN labels as potential test packages."
                )
                return gn_labels
            return set()

        # 3. Find test packages in tests.json
        try:
            with open(tests_json_path) as f:
                tests_data = json.load(f)
        except json.JSONDecodeError:
            self.log_func(f"ERROR: Failed to parse corrupted {tests_json_path}")
            return gn_labels
        except (ValueError, OSError) as e:
            self.log_func(f"ERROR: Failed to read {tests_json_path}: {e}")
            return gn_labels

        # Filter tests that are in the same directory as our GN labels
        candidate_tests = []
        label_dirs = set()
        for label in gn_labels:
            directory, colon, name = label.partition(":")
            if colon == ":":
                label_dirs.add(directory)

        for test_entry in tests_data:
            test = test_entry.get("test", {})
            label = test.get("label", "")
            if not label:
                continue

            directory, colon, name = label.partition(":")
            if colon == ":":
                if directory in label_dirs:
                    candidate_tests.append(label)

        self.log_func(
            f"Found {len(candidate_tests)} candidate tests in same directories."
        )

        final_tests = set()
        # Return all candidates in the same directory (heuristic)
        for test_label in candidate_tests:
            final_tests.add(test_label)

        return final_tests
