#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for properties.py."""

import argparse
import pathlib
import tempfile
import unittest

import properties


class PropertiesTest(unittest.TestCase):
    def test_str_to_bool(self) -> None:
        self.assertTrue(properties.str_to_bool("true"))
        self.assertTrue(properties.str_to_bool("1"))
        self.assertTrue(properties.str_to_bool("yes"))
        self.assertTrue(properties.str_to_bool(True))
        self.assertFalse(properties.str_to_bool("false"))
        self.assertFalse(properties.str_to_bool("0"))
        self.assertFalse(properties.str_to_bool("no"))
        self.assertFalse(properties.str_to_bool(False))
        self.assertFalse(properties.str_to_bool("maybe", default=False))
        self.assertTrue(properties.str_to_bool("maybe", default=True))
        with self.assertRaises(argparse.ArgumentTypeError):
            properties.str_to_bool("maybe")

    def test_parse_properties(self) -> None:
        test_cases = [
            ("key=value\n", {"key": "value"}),
            ("  key  =  value  \n", {"key": "value"}),
            ('export key="value"\n', {"key": "value"}),
            ("export key='value'\n", {"key": "value"}),
            ('key="value" # comments!\n', {"key": "value"}),
            ("=empty_key\nkey=value\n", {"key": "value"}),
            (
                "# comments\n# key=ignored\n// comments\n\nkey=value\n",
                {"key": "value"},
            ),
            ("empty_val=\n", {}),
            ("invalid_value\n", {}),
            ('key="unclosed\nkey2=value\n', {"key2": "value"}),
        ]
        for content, expected in test_cases:
            self.assertEqual(
                properties.parse_properties(content),
                expected,
                f"Failed parsing: {content!r}",
            )

    def test_load_properties(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = pathlib.Path(tmpdir)
            missing = root / "missing"
            self.assertEqual(properties.load_properties(missing), {})

            config = root / "config"
            config.write_text("export FOO='bar'\nBAZ=1 # comment\n")
            self.assertEqual(
                properties.load_properties(config),
                {"FOO": "bar", "BAZ": "1"},
            )


if __name__ == "__main__":
    unittest.main()
