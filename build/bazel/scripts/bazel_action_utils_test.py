#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import stdio_redirection
from bazel_action_utils import BazelStderrDebugLineFilter, find_prefix_in_input


class FindPrefixInInputTest(unittest.TestCase):
    def test_find_prefix_in_input(self) -> None:
        TEST_CASES = [
            ("foo", "-------", (0, 7)),  # no match
            ("foo", "foo----", (2, 0)),  # full matches
            ("foo", "--foo--", (2, 2)),
            ("foo", "----foo", (2, 4)),
            ("foo", "-----fo", (1, 5)),  # partial matches
            ("foo", "------f", (1, 6)),
        ]
        for prefix, input, expected in TEST_CASES:
            self.assertEqual(
                find_prefix_in_input(prefix, input),
                expected,
                msg=f"For prefix={prefix} and input={input}",
            )


class BazelStderrDebugLineFilterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.output = stdio_redirection.BytesOutputSink()

    def test_no_filtering(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(self.output)
        self.assertTrue(filter_sink.write(b"foooo\nsomethingDEBUG: bar"))
        self.assertEqual(self.output.data, b"foooo\nsomething")
        self.assertTrue(filter_sink.write(b"\nfinish"))
        self.assertEqual(
            self.output.data, b"foooo\nsomethingDEBUG: bar\nfinish"
        )

    def test_no_filtering_colored(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(self.output)
        self.assertTrue(
            filter_sink.write(b"foooo\nsomething\x1b[33mDEBUG: \x1b[0mbar")
        )
        self.assertEqual(self.output.data, b"foooo\nsomething")
        self.assertTrue(filter_sink.write(b"\nfinish"))
        self.assertEqual(
            self.output.data,
            b"foooo\nsomething\x1b[33mDEBUG: \x1b[0mbar\nfinish",
        )

    def test_no_filtering_bad_color(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(self.output)
        self.assertTrue(
            filter_sink.write(b"foooo\nsomething\x1b[31mDEBUG: \x1b[0mbar")
        )
        self.assertEqual(self.output.data, b"foooo\nsomething\x1b[31m")
        self.assertTrue(filter_sink.write(b"\nfinish"))
        self.assertEqual(
            self.output.data,
            b"foooo\nsomething\x1b[31mDEBUG: \x1b[0mbar\nfinish",
        )

    def test_partial_writes(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(self.output)
        self.assertTrue(filter_sink.write(b"foooo\nsomethingDEB"))
        self.assertEqual(self.output.data, b"foooo\nsomething")
        self.assertTrue(filter_sink.write(b"UG: bar"))
        self.assertEqual(self.output.data, b"foooo\nsomething")
        self.assertTrue(filter_sink.write(b"\nfinish"))
        self.assertEqual(
            self.output.data, b"foooo\nsomethingDEBUG: bar\nfinish"
        )

    def test_with_filtering_all(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(self.output, lambda x: True)
        self.assertTrue(
            filter_sink.write(
                b"foooo\nsomething\nDEBUG: bar\nsomething else\nDEBUG: zoo\n"
            )
        )
        self.assertEqual(
            self.output.data, b"foooo\nsomething\nsomething else\n"
        )

    def test_with_filtering_some(self) -> None:
        filter_sink = BazelStderrDebugLineFilter(
            self.output, lambda x: b"SKIP" in x
        )
        self.assertTrue(
            filter_sink.write(
                b"foooo\nsomething\nDEBUG: KEEP ME\nsomething else\nDEBUG: SKIP ME\n"
            )
        )
        self.assertEqual(
            self.output.data,
            b"foooo\nsomething\nDEBUG: KEEP ME\nsomething else\n",
        )


if __name__ == "__main__":
    unittest.main()
