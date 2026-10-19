#!/usr/bin/env fuchsia-vendored-python
#
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import argparse
import contextlib
import io
import os
import re
import subprocess
import tempfile
import unittest
from unittest import mock

import dtc

# Captured from dtc 1.7.0.
DUPLICATE_LABEL_STDERR = """\
{path}:6.9-8.4: ERROR (duplicate_label): /b: Duplicate label 'foo' on /b and /a
Warning: Input tree has errors, output forced
"""

SAME_LINE_WARNING_STDERR = """\
{path}:4.3-13: Warning (reg_format): /c@1:reg: property has invalid length
out.dtb: Warning (simple_bus_reg): Failed prerequisite 'reg_format'
"""

SYNTAX_ERROR_STDERR = """\
Error: {path}:4.1-2 syntax error
FATAL ERROR: Unable to parse input tree
"""

DTS_SOURCE = """\
/dts-v1/;
/ {
\tfoo: a {
\t\treg = <2>;
\t};
\tfoo: b {
\t\ty = <2>;
\t};
};
"""


class TargetRegexTest(unittest.TestCase):
    def test_single_position(self) -> None:
        match = re.search(dtc.TARGET_REGEX, "a.dts:3.6: Warning (x): msg")
        assert match is not None
        self.assertEqual(
            match.groups(), ("a.dts", "3", "6", "Warning (x): msg")
        )

    def test_same_line_range(self) -> None:
        match = re.search(dtc.TARGET_REGEX, "a.dts:4.3-13: Warning (x): msg")
        assert match is not None
        self.assertEqual(
            match.groups(), ("a.dts", "4", "3", "Warning (x): msg")
        )

    def test_multi_line_range(self) -> None:
        match = re.search(dtc.TARGET_REGEX, "a.dts:6.9-8.4: ERROR (x): msg")
        assert match is not None
        self.assertEqual(match.groups(), ("a.dts", "6", "9", "ERROR (x): msg"))

    def test_parse_error(self) -> None:
        match = re.search(dtc.TARGET_REGEX, "Error: a.dts:4.1-2 syntax error")
        assert match is not None
        self.assertEqual(match.groups(), ("a.dts", "4", "1", "syntax error"))

    def test_no_position(self) -> None:
        for line in [
            "out.dtb: Warning (simple_bus_reg): Failed prerequisite 'reg'",
            "Warning: Input tree has errors, output forced",
            "FATAL ERROR: Unable to parse input tree",
        ]:
            self.assertIsNone(re.search(dtc.TARGET_REGEX, line), line)


class RunDtcTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.src = os.path.join(self.tmpdir.name, "board.dts")
        with open(self.src, "w") as f:
            f.write(DTS_SOURCE)
        self.dest = os.path.join(self.tmpdir.name, "board.dtb")
        self.dep = os.path.join(self.tmpdir.name, "board.d")
        self.includes = os.path.join(self.tmpdir.name, "includes.txt")
        with open(self.includes, "w") as f:
            f.write("gen/include_dir\n")

    def compile_with(
        self, returncode: int, stderr: str
    ) -> tuple[bool, list[str], str]:
        """Runs dts_compile against a fake dtc that writes its output file.

        Returns the result, the dtc command line, and the captured stderr.
        """
        calls: list[list[str]] = []

        def fake_run(
            cmd_args: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[str]:
            calls.append(cmd_args)
            with open(self.dest, "w") as f:
                f.write("forced output")
            return subprocess.CompletedProcess(
                cmd_args, returncode, "", stderr.format(path=self.src)
            )

        with open(self.includes) as includes_file:
            args = argparse.Namespace(
                dtc_path="dtc",
                src_file=self.src,
                dest_file=self.dest,
                dep_file=self.dep,
                includes_file=includes_file,
            )
            out = io.StringIO()
            err = io.StringIO()
            with mock.patch.object(
                subprocess, "run", side_effect=fake_run
            ), contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
                result = dtc.dts_compile(args, ["-Wno-foo"])

        self.assertEqual(out.getvalue(), "")
        self.assertEqual(len(calls), 1)
        return result, calls[0], err.getvalue()

    def test_success(self) -> None:
        result, cmd, err = self.compile_with(0, "")
        self.assertFalse(result)
        self.assertEqual(err, "")
        self.assertTrue(os.path.exists(self.dest))
        self.assertEqual(
            cmd,
            [
                "dtc",
                "--force",
                "--out-dependency=" + self.dep,
                "--out=" + self.dest,
                "--include=gen/include_dir",
                "-Wno-foo",
                self.src,
            ],
        )

    def test_forced_error_fails_with_context(self) -> None:
        result, _, err = self.compile_with(0, DUPLICATE_LABEL_STDERR)
        self.assertTrue(result)
        self.assertFalse(os.path.exists(self.dest))
        # Context around line 6 of the source, with a marker under column 9.
        self.assertIn("    6: \tfoo: b {", err)
        self.assertIn(
            " " * (dtc.LINENUM_WIDTH + 9)
            + f"{dtc.RED}^--- ERROR (duplicate_label)",
            err,
        )
        self.assertIn("Returning failure", err)

    def test_warning_fails_with_context(self) -> None:
        result, _, err = self.compile_with(0, SAME_LINE_WARNING_STDERR)
        self.assertTrue(result)
        self.assertFalse(os.path.exists(self.dest))
        self.assertIn("    4: \t\treg = <2>;", err)
        self.assertIn(
            " " * (dtc.LINENUM_WIDTH + 3)
            + f"{dtc.RED}^--- Warning (reg_format)",
            err,
        )
        self.assertIn("Returning failure", err)

    def test_dtc_failure(self) -> None:
        result, _, err = self.compile_with(1, SYNTAX_ERROR_STDERR)
        self.assertTrue(result)
        self.assertFalse(os.path.exists(self.dest))
        self.assertIn(
            " " * (dtc.LINENUM_WIDTH + 1) + f"{dtc.RED}^--- syntax error",
            err,
        )
        # dtc's own failure needs no extra explanation.
        self.assertNotIn("Returning failure", err)

    def test_missing_source_continues(self) -> None:
        stderr = (
            "missing.dts:1.1-2: Warning (x): first\n"
            "{path}:4.3-13: Warning (y): second\n"
        )
        result, _, err = self.compile_with(0, stderr)
        self.assertTrue(result)
        self.assertIn("Error: 'missing.dts' not found.", err)
        self.assertIn(f"{dtc.RED}^--- Warning (y): second", err)

    def test_decompile_forces_output(self) -> None:
        args = argparse.Namespace(
            dtc_path="dtc", src_file="in.dtb", dest_file=self.dest
        )
        with mock.patch.object(
            subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "", ""),
        ) as run:
            self.assertFalse(dtc.dtb_decompile(args, []))
        self.assertEqual(
            run.call_args.args[0],
            [
                "dtc",
                "--force",
                "--sort",
                "--in-format=dtb",
                "--out-format=dts",
                "--out=" + self.dest,
                "in.dtb",
            ],
        )


if __name__ == "__main__":
    unittest.main()
