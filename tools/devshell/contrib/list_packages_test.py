#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import unittest
from collections.abc import Iterable

import list_packages


class TestPrintPackages(unittest.TestCase):
    def validate(self, input: Iterable[str], output: str) -> None:
        f = io.StringIO()
        with contextlib.redirect_stdout(f):
            list_packages.print_packages(input)

        self.assertEqual(f.getvalue(), output)

    def test_single_package(self) -> None:
        self.validate(["package"], "package\n")

    def test_multiple_packages(self) -> None:
        self.validate(
            ["package0", "package2", "package1", "package3"],
            """package0
package1
package2
package3
""",
        )


if __name__ == "__main__":
    unittest.main()
