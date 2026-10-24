#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for rbe_settings.py."""

import json
import tempfile
import unittest
from pathlib import Path

from build.rbe import rbe_settings


class RbeSettingsTest(unittest.TestCase):
    """Tests the properties and validation of the RbeSettings dataclass."""

    def test_from_dict_success(self) -> None:
        data = {
            "bazel_enable": True,
            "bazel_exec_strategy": "remote",
            "bazel_download_outputs": "toplevel",
            "cxx_download_objects": True,
            "cxx_enable": True,
            "cxx_exec_strategy": "remote",
            "cxx_minimalist_wrapper": True,
            "link_download_unstripped_outputs": True,
            "link_enable": True,
            "link_exec_strategy": "remote",
            "rust_download_rlibs": True,
            "rust_download_unstripped_binaries": True,
            "rust_enable": False,
            "rust_exec_strategy": "remote",
            "needs_reproxy": True,
            "needs_auth": True,
        }
        settings = rbe_settings.RbeSettings.from_dict(data)
        self.assertTrue(settings.bazel_enable)
        self.assertEqual(settings.bazel_exec_strategy, "remote")
        self.assertEqual(settings.bazel_download_outputs, "toplevel")
        self.assertTrue(settings.cxx_download_objects)
        self.assertTrue(settings.cxx_enable)
        self.assertEqual(settings.cxx_exec_strategy, "remote")
        self.assertTrue(settings.cxx_minimalist_wrapper)
        self.assertTrue(settings.link_download_unstripped_outputs)
        self.assertTrue(settings.link_enable)
        self.assertEqual(settings.link_exec_strategy, "remote")
        self.assertTrue(settings.rust_download_rlibs)
        self.assertTrue(settings.rust_download_unstripped_binaries)
        self.assertFalse(settings.rust_enable)
        self.assertEqual(settings.rust_exec_strategy, "remote")
        self.assertTrue(settings.needs_reproxy)
        self.assertTrue(settings.needs_auth)
        self.assertTrue(settings.rbe_enabled)
        self.assertTrue(settings.rs_enabled)

    def test_from_dict_missing_field_raises(self) -> None:
        data = {
            "bazel_enable": True,
            "bazel_exec_strategy": "remote",
            "bazel_download_outputs": "toplevel",
            "cxx_download_objects": True,
            "cxx_enable": True,
            "cxx_exec_strategy": "remote",
            "cxx_minimalist_wrapper": True,
            "link_download_unstripped_outputs": True,
            # "link_enable" is missing
            "link_exec_strategy": "remote",
            "rust_download_rlibs": True,
            "rust_download_unstripped_binaries": True,
            "rust_enable": False,
            "rust_exec_strategy": "remote",
            "needs_reproxy": True,
            "needs_auth": True,
        }
        with self.assertRaises(KeyError) as ctx:
            rbe_settings.RbeSettings.from_dict(data)
        self.assertIn(
            "link_enable",
            str(ctx.exception),
        )


class LoadTest(unittest.TestCase):
    """Tests the loading of RBE settings from path or build directory."""

    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.temp_dir = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_load_from_path_success(self) -> None:
        json_file = self.temp_dir / "rbe_settings.json"
        json_file.write_text(
            json.dumps(
                {
                    "final": {
                        "bazel_enable": False,
                        "bazel_exec_strategy": "",
                        "bazel_download_outputs": "all",
                        "cxx_download_objects": False,
                        "cxx_enable": False,
                        "cxx_exec_strategy": "",
                        "cxx_minimalist_wrapper": False,
                        "link_download_unstripped_outputs": False,
                        "link_enable": False,
                        "link_exec_strategy": "",
                        "rust_download_rlibs": False,
                        "rust_download_unstripped_binaries": False,
                        "rust_enable": False,
                        "rust_exec_strategy": "",
                        "needs_reproxy": False,
                        "needs_auth": False,
                    }
                }
            )
        )
        settings = rbe_settings._load_from_path(json_file)
        self.assertFalse(settings.rbe_enabled)
        self.assertFalse(settings.rs_enabled)

    def test_load_from_path_missing_file_raises(self) -> None:
        non_existent = self.temp_dir / "missing.json"
        with self.assertRaises(ValueError) as ctx:
            rbe_settings._load_from_path(non_existent)
        self.assertIn("Failed to read/parse RBE settings", str(ctx.exception))

    def test_load_from_build_dir_success(self) -> None:
        # Create a build directory with rbe_settings.json inside it
        (self.temp_dir / rbe_settings._SETTINGS_FILE).write_text(
            json.dumps(
                {
                    "final": {
                        "bazel_enable": True,
                        "bazel_exec_strategy": "remote",
                        "bazel_download_outputs": "toplevel",
                        "cxx_download_objects": True,
                        "cxx_enable": True,
                        "cxx_exec_strategy": "remote",
                        "cxx_minimalist_wrapper": True,
                        "link_download_unstripped_outputs": True,
                        "link_enable": True,
                        "link_exec_strategy": "remote",
                        "rust_download_rlibs": True,
                        "rust_download_unstripped_binaries": True,
                        "rust_enable": True,
                        "rust_exec_strategy": "remote",
                        "needs_reproxy": True,
                        "needs_auth": True,
                    }
                }
            )
        )
        settings = rbe_settings.load(self.temp_dir)
        self.assertTrue(settings.rbe_enabled)


class ExistsTest(unittest.TestCase):
    """Tests the existence check of RBE settings files."""

    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.temp_dir = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_exists_false(self) -> None:
        self.assertFalse(rbe_settings.exists(self.temp_dir))

    def test_exists_true(self) -> None:
        (self.temp_dir / rbe_settings._SETTINGS_FILE).write_text("{}")
        self.assertTrue(rbe_settings.exists(self.temp_dir))


class FakeTest(unittest.TestCase):
    """Tests the in-memory fake() factory helper function."""

    def test_fake_defaults(self) -> None:
        settings = rbe_settings.fake()
        self.assertFalse(settings.rbe_enabled)
        self.assertFalse(settings.rs_enabled)
        self.assertFalse(settings.cxx_enable)
        self.assertTrue(settings.cxx_download_objects)

    def test_fake_overrides(self) -> None:
        settings = rbe_settings.fake(
            cxx_enable=True,
            cxx_exec_strategy="remote",
            needs_reproxy=True,
        )
        self.assertTrue(settings.rbe_enabled)
        self.assertTrue(settings.cxx_enable)
        self.assertEqual(settings.cxx_exec_strategy, "remote")
        self.assertTrue(settings.needs_reproxy)

    def test_fake_unknown_overrides_raises_value_error(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            rbe_settings.fake(nonexistent_key=True)
        self.assertIn("nonexistent_key", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
