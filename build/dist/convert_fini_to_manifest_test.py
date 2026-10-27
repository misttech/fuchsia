# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for convert_fini_to_manifest.py."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from build.dist import convert_fini_to_manifest


class ConvertFiniToManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir_obj = tempfile.TemporaryDirectory()
        self.temp_dir = Path(self.temp_dir_obj.name)

    def tearDown(self) -> None:
        self.temp_dir_obj.cleanup()

    def test_main(self) -> None:
        fini_file = self.temp_dir / "input.fini"
        fini_file.write_text(
            "bin/app=path/to/binary\nlib/libfoo.so=path/to/libfoo.so\n"
        )
        output_file = self.temp_dir / "output.json"

        test_args = [
            "convert_fini_to_manifest.py",
            "--input",
            str(fini_file),
            "--output",
            str(output_file),
            "--label",
            "//src:target",
        ]
        with mock.patch.object(sys, "argv", test_args):
            convert_fini_to_manifest.main()

        self.assertTrue(output_file.is_file())
        entries = json.loads(output_file.read_text())
        self.assertEqual(len(entries), 2)
        self.assertEqual(
            entries[0],
            {
                "destination": "bin/app",
                "source": "path/to/binary",
                "label": "//src:target",
            },
        )
        self.assertEqual(
            entries[1],
            {
                "destination": "lib/libfoo.so",
                "source": "path/to/libfoo.so",
                "label": "//src:target",
            },
        )


if __name__ == "__main__":
    unittest.main()
