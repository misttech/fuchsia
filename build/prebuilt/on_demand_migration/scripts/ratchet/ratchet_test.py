#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ratchet.py."""

from __future__ import annotations

import contextlib
import io
import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import ratchet


class RatchetTest(unittest.TestCase):
    """Tests baseline ratchet enforcement, CLI flags, and allowlist generation."""

    def test_check_files_allows_within_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            allowlist_path = root / ratchet.ALLOWLIST_RELATIVE_PATH
            allowlist_path.parent.mkdir(parents=True, exist_ok=True)
            allowlist_path.write_text(
                json.dumps({"src/legacy/BUILD.gn": 1}), encoding="utf-8"
            )

            target_file = root / "src" / "legacy" / "BUILD.gn"
            target_file.parent.mkdir(parents=True, exist_ok=True)
            target_file.write_text(
                'inputs = [ "//prebuilt/third_party/foo" ]\n',
                encoding="utf-8",
            )
            self.assertSequenceEqual(
                ratchet.check_files(
                    root, ["src/legacy/BUILD.gn"], allowlist_path
                ),
                (),
            )

    def test_check_files_reports_all_excess_violations(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            allowlist_path = root / ratchet.ALLOWLIST_RELATIVE_PATH
            allowlist_path.parent.mkdir(parents=True, exist_ok=True)
            allowlist_path.write_text(
                json.dumps({"src/legacy/BUILD.gn": 1}), encoding="utf-8"
            )

            target_file = root / "src" / "legacy" / "BUILD.gn"
            target_file.parent.mkdir(parents=True, exist_ok=True)
            target_file.write_text(
                (
                    "inputs = [\n"
                    '  "//prebuilt/third_party/foo",\n'
                    '  "//prebuilt/third_party/bar",\n'
                    '  "//prebuilt/third_party/baz",\n'
                    "]\n"
                ),
                encoding="utf-8",
            )
            findings = ratchet.check_files(
                root, ["src/legacy/BUILD.gn"], allowlist_path
            )
            self.assertEqual(len(findings), 2)
            self.assertEqual((findings[0].line, findings[0].col), (3, 3))
            self.assertEqual((findings[1].line, findings[1].col), (4, 3))

    def test_check_files_remediation_messages_match_file_types(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            allowlist_path = root / ratchet.ALLOWLIST_RELATIVE_PATH

            cases = (
                ("src/foo/BUILD.gn", 'x = "//prebuilt/a"\n'),
                ("src/foo/BUILD.bazel", 'x = "//prebuilt/a"\n'),
                ("tools/devshell/foo", 'exec "//prebuilt/a"\n'),
                ("scripts/helper.sh", 'exec "//prebuilt/a"\n'),
                ("scripts/foo.py", 'X = "//prebuilt/a"\n'),
                ("src/foo/driver.cc", '#include "prebuilt/a.h"\n'),
            )
            for relative_path, content in cases:
                full_path = root / relative_path
                full_path.parent.mkdir(parents=True, exist_ok=True)
                full_path.write_text(content, encoding="utf-8")

            findings = ratchet.check_files(
                root, [path for path, _ in cases], allowlist_path
            )
            messages_by_path = {
                finding.filepath: finding.message for finding in findings
            }
            self.assertIn(
                "//build/prebuilt/cipd_prebuilt.gni",
                messages_by_path["src/foo/BUILD.gn"],
            )
            self.assertIn(
                "cipd_prebuilt_file()",
                messages_by_path["src/foo/BUILD.bazel"],
            )
            self.assertIn(
                "#### PREBUILTS=",
                messages_by_path["tools/devshell/foo"],
            )
            self.assertIn(
                "devshell scripts",
                messages_by_path["scripts/helper.sh"],
            )
            self.assertIn(
                "build/tool arguments",
                messages_by_path["scripts/foo.py"],
            )
            self.assertIn(
                "include_dirs",
                messages_by_path["src/foo/driver.cc"],
            )

    def test_update_allowlists_writes_fuchsia_and_vendor_counts(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fuchsia_dir = pathlib.Path(temp_dir)
            (fuchsia_dir / "src").mkdir(parents=True)
            (fuchsia_dir / "src" / "BUILD.gn").write_text(
                'inputs = [ "//prebuilt/third_party/foo" ]\n',
                encoding="utf-8",
            )
            vendor_dir = fuchsia_dir / "vendor" / "google"
            (vendor_dir / "tools").mkdir(parents=True)
            (vendor_dir / "tools" / "BUILD.gn").write_text(
                'inputs = [ "//prebuilt/vendor/bar", "//prebuilt/vendor/baz" ]\n',
                encoding="utf-8",
            )

            with mock.patch.object(
                subprocess,
                "check_output",
                autospec=True,
                side_effect=["src/BUILD.gn\n", "tools/BUILD.gn\n"],
            ):
                ratchet.update_allowlists(fuchsia_dir)

            root_allowlist = json.loads(
                (fuchsia_dir / ratchet.ALLOWLIST_RELATIVE_PATH).read_text(
                    encoding="utf-8"
                )
            )
            vendor_allowlist = json.loads(
                (vendor_dir / ratchet.ALLOWLIST_RELATIVE_PATH).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(root_allowlist, {"src/BUILD.gn": 1})
            self.assertEqual(vendor_allowlist, {"tools/BUILD.gn": 2})

    def test_main_json_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            bad_file = root / "src" / "BUILD.gn"
            bad_file.parent.mkdir(parents=True, exist_ok=True)
            bad_file.write_text(
                'inputs = [ "//prebuilt/third_party/foo" ]\n',
                encoding="utf-8",
            )

            json_stdout = io.StringIO()
            with contextlib.redirect_stdout(json_stdout):
                exit_code = ratchet.main(
                    ["--root", str(root), "--json", "src/BUILD.gn"]
                )
            self.assertEqual(exit_code, 1)
            parsed = json.loads(json_stdout.getvalue())
            self.assertEqual(len(parsed), 1)
            self.assertEqual(parsed[0]["filepath"], "src/BUILD.gn")

    def test_main_files_from_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            bad_file = root / "src" / "BUILD.gn"
            bad_file.parent.mkdir(parents=True, exist_ok=True)
            bad_file.write_text(
                'inputs = [ "//prebuilt/third_party/foo" ]\n',
                encoding="utf-8",
            )
            manifest_file = root / "files.txt"
            manifest_file.write_text("src/BUILD.gn\n", encoding="utf-8")

            json_stdout = io.StringIO()
            with contextlib.redirect_stdout(json_stdout):
                exit_code = ratchet.main(
                    [
                        "--root",
                        str(root),
                        "--json",
                        "--files-from",
                        str(manifest_file),
                    ]
                )
            self.assertEqual(exit_code, 1)
            parsed = json.loads(json_stdout.getvalue())
            self.assertEqual(len(parsed), 1)
            self.assertEqual(parsed[0]["filepath"], "src/BUILD.gn")

    def test_main_text_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            bad_file = root / "src" / "BUILD.gn"
            bad_file.parent.mkdir(parents=True, exist_ok=True)
            bad_file.write_text(
                'inputs = [ "//prebuilt/third_party/foo" ]\n',
                encoding="utf-8",
            )

            text_stdout = io.StringIO()
            with contextlib.redirect_stdout(text_stdout):
                exit_code = ratchet.main(["--root", str(root), "src/BUILD.gn"])
            self.assertEqual(exit_code, 1)
            self.assertIn("src/BUILD.gn:1:12: error:", text_stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
