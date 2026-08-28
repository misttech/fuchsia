#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the top-level `fx agents` CLI dispatcher."""

from __future__ import annotations

import argparse
import io
import unittest
from unittest import mock

from agents import main


class MainCliTest(unittest.TestCase):
    """Test top-level argument parsing and subcommand dispatching."""

    def test_help_flag(self) -> None:
        parser = main.create_parser()
        with (
            mock.patch("sys.stdout", new_callable=io.StringIO),
            self.assertRaises(SystemExit) as cm,
        ):
            parser.parse_args(["--help"])
        self.assertEqual(cm.exception.code, 0)

    def test_no_subcommand_prints_help_and_fails(self) -> None:
        with mock.patch("sys.stderr", new_callable=io.StringIO) as mock_stderr:
            exit_code = main.main([])
            self.assertEqual(exit_code, 1)
            self.assertIn("fx agents", mock_stderr.getvalue())

    def test_subcommand_dispatch(self) -> None:
        parser = argparse.ArgumentParser(prog="fx agents")
        subparsers = parser.add_subparsers(dest="subcommand")
        mock_handler = mock.MagicMock(return_value=42)
        subparsers.add_parser("test").set_defaults(func=mock_handler)

        with mock.patch.object(main, "create_parser", return_value=parser):
            exit_code = main.main(["test"])
            self.assertEqual(exit_code, 42)
            mock_handler.assert_called_once()


if __name__ == "__main__":
    unittest.main()
