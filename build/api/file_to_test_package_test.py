# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import unittest
from pathlib import Path
from unittest import mock

import file_to_test_package


class TestFileToTestPackageFinder(unittest.TestCase):
    def setUp(self):
        self.build_dir = Path("/build/dir")
        self.fuchsia_dir = Path("/fuchsia/dir")
        self.mock_outputs = mock.Mock()
        self.mock_log = mock.Mock()
        self.finder = file_to_test_package.FileToTestPackageFinder(
            self.build_dir,
            self.fuchsia_dir,
            self.mock_outputs,
            self.mock_log,
        )

    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_find_test_packages_fast_rust(self, mock_exists, mock_open):
        source_path = "src/lib.rs"
        abs_source = self.fuchsia_dir / source_path

        def exists_side_effect(path):
            if path == self.build_dir / "rust-project.json":
                return True
            if path == self.build_dir / "tests.json":
                return True
            return False

        mock_exists.return_value = (
            True  # everything exists unless we say otherwise
        )
        mock_exists.side_effect = None

        rust_content = {
            "crates": [
                {
                    "root_module": str(abs_source),
                    "label": "//src:lib",
                }
            ]
        }
        tests_content = [
            {
                "test": {
                    "label": "//src:lib_test",
                }
            }
        ]

        mock_file = mock.Mock()
        mock_open.return_value.__enter__.return_value = mock_file

        with mock.patch("json.load") as mock_json_load:

            def json_side_effect(f):
                call_args = mock_open.call_args[0]
                path = call_args[0]
                if str(path).endswith("rust-project.json"):
                    return rust_content
                if str(path).endswith("tests.json"):
                    return tests_content
                return {}

            mock_json_load.side_effect = json_side_effect
            result = self.finder.find_test_packages_fast(source_path)
            self.assertEqual(result, {"//src:lib_test"})
            self.mock_log.assert_any_call(
                "Found 1 candidate tests in same directories."
            )

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_find_test_packages_fast_cpp_compile_commands(
        self, mock_exists, mock_open, mock_json_load
    ):
        source_path = "src/foo.cc"
        abs_source = self.fuchsia_dir / source_path

        def exists_side_effect(self_path):
            p = str(self_path)
            if "rust-project.json" in p:
                return False
            if "compile_commands.json" in p:
                return True
            if "tests.json" in p:
                return True
            return False

        mock_exists.side_effect = exists_side_effect

        cc_content = [{"file": str(abs_source), "output": "obj/src/foo.o"}]
        tests_content = [
            {
                "test": {
                    "label": "//src:foo_test",
                }
            }
        ]
        path_map = {
            "compile_commands.json": cc_content,
            "tests.json": tests_content,
        }

        def open_side_effect(path, *args, **kwargs):
            p = str(path)
            m = mock.MagicMock()
            for k, v in path_map.items():
                if p.endswith(k):
                    m.__enter__.return_value.tagged_content = v
                    return m
            return m

        mock_open.side_effect = open_side_effect

        def json_load_side_effect(f):
            return getattr(f, "tagged_content", {})

        mock_json_load.side_effect = json_load_side_effect

        self.mock_outputs.path_to_gn_label.return_value = "//src:foo"

        result = self.finder.find_test_packages_fast(source_path)

        self.assertEqual(result, {"//src:foo_test"})
        self.assertEqual(result, {"//src:foo_test"})
        self.mock_outputs.path_to_gn_label.assert_called_with("obj/src/foo.o")

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_find_test_packages_fast_cpp_o_attached(
        self, mock_exists, mock_open, mock_json_load
    ):
        source_path = "src/complex.cc"
        abs_source = self.fuchsia_dir / source_path

        mock_exists.return_value = True

        cc_content = [
            {
                "file": str(abs_source),
                "command": "clang++ -c src/complex.cc -oobj/src/complex.o",
                "output": "",
            }
        ]
        tests_content = [{"test": {"label": "//src:complex_test"}}]

        mock_file = mock.MagicMock()
        mock_open.return_value.__enter__.return_value = mock_file

        def json_load_side_effect(f):
            call_args = mock_open.call_args[0]
            path = str(call_args[0])
            if path.endswith("compile_commands.json"):
                return cc_content
            if path.endswith("tests.json"):
                return tests_content
            return {"crates": []}

        mock_json_load.side_effect = json_load_side_effect

        self.mock_outputs.path_to_gn_label.return_value = "//src:complex"

        result = self.finder.find_test_packages_fast("src/complex.cc")
        self.assertEqual(result, {"//src:complex_test"})
        self.mock_outputs.path_to_gn_label.assert_called_with(
            "obj/src/complex.o"
        )

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_find_test_packages_fast_cpp_multiple_o(
        self, mock_exists, mock_open, mock_json_load
    ):
        source_path = "src/multi.cc"
        abs_source = self.fuchsia_dir / source_path

        mock_exists.return_value = True

        cc_content = [
            {
                "file": str(abs_source),
                "command": "clang++ -c src/multi.cc -o obj/src/fake.o -o obj/src/multi.o",
                "output": "",
            }
        ]
        tests_content = [{"test": {"label": "//src:multi_test"}}]

        mock_file = mock.MagicMock()
        mock_open.return_value.__enter__.return_value = mock_file

        def json_load_side_effect(f):
            call_args = mock_open.call_args[0]
            path = str(call_args[0])
            if path.endswith("compile_commands.json"):
                return cc_content
            if path.endswith("tests.json"):
                return tests_content
            return {"crates": []}

        mock_json_load.side_effect = json_load_side_effect

        self.mock_outputs.path_to_gn_label.return_value = "//src:multi"

        result = self.finder.find_test_packages_fast("src/multi.cc")
        self.assertEqual(result, {"//src:multi_test"})
        self.mock_outputs.path_to_gn_label.assert_called_with("obj/src/multi.o")

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_missing_tests_json_fallback(
        self, mock_exists, mock_open, mock_json_load
    ):
        source_path = "src/lib.rs"
        abs_source = self.fuchsia_dir / source_path

        # rust-project.json exists, tests.json does NOT
        def exists_side_effect(path):
            p = str(path)
            if "rust-project.json" in p:
                return True
            if "tests.json" in p:
                return False
            return False

        def simple_exists(obj):
            p = str(obj)
            return "rust-project.json" in p

        mock_exists.side_effect = simple_exists

        # Content
        rust_content = {
            "crates": [
                {
                    "root_module": str(abs_source),
                    "label": "//src:lib",
                }
            ]
        }

        mock_open.side_effect = [mock.MagicMock()]  # rust-project.json
        mock_json_load.return_value = rust_content

        result = self.finder.find_test_packages_fast(source_path)

        self.assertEqual(result, {"//src:lib"})
        self.assertTrue(
            any(
                "WARNING" in str(arg) and "tests.json not found" in str(arg)
                for arg in self.mock_log.call_args[0]
            )
        )

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_corrupted_tests_json(self, mock_exists, mock_open, mock_json_load):
        source_path = "src/lib.rs"
        abs_source = self.fuchsia_dir / source_path
        mock_exists.return_value = True
        rust_content = {
            "crates": [
                {
                    "root_module": str(abs_source),
                    "label": "//src:lib",
                }
            ]
        }

        mock_json_load.side_effect = [
            rust_content,
            json.JSONDecodeError("Expecting value", "doc", 0),
        ]

        result = self.finder.find_test_packages_fast(source_path)

        # Should fall back to found labels
        self.assertEqual(result, {"//src:lib"})
        self.mock_log.assert_called_with(mock.ANY)
        found_error = False
        for call_args in self.mock_log.call_args_list:
            if "ERROR: Failed to parse corrupted" in str(call_args):
                found_error = True
                break
        self.assertTrue(found_error, "Did not find expected error log")

    @mock.patch("json.load")
    @mock.patch("builtins.open")
    @mock.patch("pathlib.Path.exists", autospec=True)
    def test_corrupted_rust_project_json(
        self, mock_exists, mock_open, mock_json_load
    ):
        source_path = "src/lib.rs"
        self.fuchsia_dir / source_path
        mock_exists.return_value = True

        mock_file = mock.MagicMock()
        mock_open.return_value.__enter__.return_value = mock_file

        def json_side_effect(f):
            call_args = mock_open.call_args[0]
            path = str(call_args[0])
            if "rust-project.json" in path:
                raise json.JSONDecodeError("Expecting value", "doc", 0)
            return []

        mock_json_load.side_effect = json_side_effect

        result = self.finder.find_test_packages_fast(source_path)

        self.assertEqual(
            result, set()
        )  # Should handle gracefully and return empty set (or search other files, but here we mock only one)

        # Verify log called
        self.mock_log.assert_called_with(mock.ANY)
        found_error = False
        for call_args in self.mock_log.call_args_list:
            if "rust-project.json search failed" in str(call_args):
                found_error = True
                break
        self.assertTrue(
            found_error, "Did not find expected rust-project.json failure log"
        )


if __name__ == "__main__":
    unittest.main()
