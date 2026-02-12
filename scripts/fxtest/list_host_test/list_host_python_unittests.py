# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import sys
import unittest
import zipimport


def load_tests_from_file(file_path: str) -> list[str]:
    """Loads test case names from a file using unittest discovery."""
    if not os.path.exists(file_path):
        print(f"File not found: {file_path}", file=sys.stderr)
        return []

    # Assume the test binary is a PYZ archive.
    try:
        # Add the PYZ to sys.path so that imports within the module work.
        sys.path.insert(0, file_path)

        importer = zipimport.zipimporter(file_path)

        modules = []
        try:
            # Try to load the module with the same name as the file
            module_name = os.path.splitext(os.path.basename(file_path))[0]
            modules.append(importer.load_module(module_name))
        except ImportError:
            # Fallback: list .py files in the PYZ
            import zipfile

            with zipfile.ZipFile(file_path, "r") as zf:
                for name in zf.namelist():
                    if name.endswith(".py") and not name.startswith("__"):
                        # Convert path to module name
                        mod_name = name.replace("/", ".")[:-3]
                        try:
                            modules.append(importer.load_module(mod_name))
                        except Exception:
                            pass

        loader = unittest.TestLoader()
        suite = unittest.TestSuite()
        for module in modules:
            suite.addTests(loader.loadTestsFromModule(module))

    except Exception as e:
        print(f"Error reading file {file_path}: {e}", file=sys.stderr)
        return []

    test_names = []

    # TestSuite can contain TestSuites or TestCases. Flatten it.
    def flatten_suite(s: unittest.TestSuite) -> None:
        for item in s:
            if isinstance(item, unittest.TestCase):
                # We want to extract "TestName.TestCaseName".
                # item.__class__.__name__ is `TestName`
                # item._testMethodName is `TestCaseName`
                test_names.append(
                    f"{item.__class__.__name__}.{item._testMethodName}"
                )
            elif isinstance(item, unittest.TestSuite):
                # Unittest can have nested TestSuites.
                flatten_suite(item)

    flatten_suite(suite)
    return test_names


def main() -> None:
    parser = argparse.ArgumentParser(description="List host tests from files.")
    parser.add_argument(
        "--list_tests",
        help="Path to the test file to list tests from.",
        required=True,
    )
    args = parser.parse_args()

    file_path = args.list_tests.strip()
    if not file_path:
        return

    tests = load_tests_from_file(file_path)
    if tests:
        print("\n".join(tests))


if __name__ == "__main__":
    main()
