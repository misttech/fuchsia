# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from shell_utils import ShellCommand


class TestShellCommand(unittest.TestCase):
    def test_split_simple(self):
        cmd = ShellCommand("echo a && echo b")
        parts = cmd.split()
        self.assertEqual(
            parts, [ShellCommand("echo a"), ShellCommand("echo b")]
        )

    def test_split_quoted_operator(self):
        cmd = ShellCommand("echo '&&' || echo b")
        parts = cmd.split()
        self.assertEqual(
            parts, [ShellCommand("echo '&&'"), ShellCommand("echo b")]
        )

    def test_split_chained(self):
        cmd = ShellCommand("a && b || c && d")
        parts = cmd.split()
        self.assertEqual(
            parts,
            [
                ShellCommand("a"),
                ShellCommand("b"),
                ShellCommand("c"),
                ShellCommand("d"),
            ],
        )

    def test_split_custom_separators(self):
        cmd = ShellCommand("a ; b ; c")
        parts = cmd.split(separators={";"})
        self.assertEqual(
            parts, [ShellCommand("a"), ShellCommand("b"), ShellCommand("c")]
        )

    def test_split_no_whitespace(self):
        cmd = ShellCommand("echo '&&'&&echo 'b'||c")
        parts = cmd.split()
        self.assertEqual(
            parts,
            [
                ShellCommand("echo '&&'"),
                ShellCommand("echo 'b'"),
                ShellCommand("c"),
            ],
        )

    def test_unwrap_simple(self):
        cmd = ShellCommand("wrapper -- inner_cmd arg")
        unwrapped = cmd.unwrap()
        self.assertEqual(unwrapped, ShellCommand("inner_cmd arg"))

    def test_unwrap_nested(self):
        cmd = ShellCommand("w1 -- w2 -- inner")
        unwrapped = cmd.unwrap()
        self.assertEqual(unwrapped, ShellCommand("w2 -- inner"))

    def test_unwrap_with_flags(self):
        cmd = ShellCommand("wrapper --opt -- inner --flag")
        unwrapped = cmd.unwrap()
        self.assertEqual(unwrapped, ShellCommand("inner --flag"))

    def test_unwrap_quotes(self):
        cmd = ShellCommand("wrapper -- 'inner --foo'")
        unwrapped = cmd.unwrap()
        self.assertEqual(unwrapped, ShellCommand("'inner --foo'"))

    def test_unwrap_empty_after_separator(self):
        cmd = ShellCommand("wrapper --")
        unwrapped = cmd.unwrap()
        self.assertEqual(unwrapped, ShellCommand(""))

    def test_unwrap_no_separator(self):
        cmd = ShellCommand("echo hello")
        with self.assertRaises(ValueError):
            cmd.unwrap()


if __name__ == "__main__":
    unittest.main()
