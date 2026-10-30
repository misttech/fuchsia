#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import sys
import unittest
from pathlib import Path

# Add parent directory to path so we can import commit_msg_checker
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import commit_msg_checker


class TestCommitMessageChecker(unittest.TestCase):
    def test_valid_message(self) -> None:
        msg = (
            "Add commit message checker\n\n"
            "This change implements a commit message checker.\n\n"
            "Bug: 12345\n"
            "Test: Added unit tests.\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_subject_line_at_limit_for_warning(self) -> None:
        subject = "A" * 65
        msg = f"{subject}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_subject_line_just_above_limit_for_warning(self) -> None:
        long_subject = "A" * 66
        msg = f"{long_subject}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertTrue(
            any(
                "Subject line exceeds 65 characters" in f["message"]
                for f in findings
            )
        )

    def test_subject_line_too_long_exempt_revert(self) -> None:
        long_subject = 'Revert "Subject that is extremely long and exceeds the typical 65 character limit normally"'
        msg = f"{long_subject}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_subject_line_too_long_exempt_reland(self) -> None:
        long_subject = 'Reland "Subject that is extremely long and exceeds the typical 65 character limit normally"'
        msg = f"{long_subject}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_subject_line_too_long_exempt_autosquash(self) -> None:
        for prefix in ("fixup!", "squash!", "amend!", "fixup! fixup!"):
            with self.subTest(prefix=prefix):
                long_subject = f"{prefix} Subject that is extremely long and exceeds the typical 65 character limit"
                msg = f"{long_subject}\n\nBug: 1\nTest: 1"
                findings = commit_msg_checker.check_commit_message(msg)
                self.assertEqual(findings, [])

    def test_body_line_at_limit(self) -> None:
        body = "A" * 72
        msg = f"Subject line\n\n{body}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_body_line_just_above_limit(self) -> None:
        long_body = "A" * 73
        msg = f"Subject line\n\n{long_body}\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertTrue(
            any(
                "Line 3 exceeds 72 characters (73 chars). Wrap body text at 72 characters."
                in f["message"]
                for f in findings
            )
        )

    def test_body_line_too_long_ignores_urls(self) -> None:
        long_url = "https://fuchsia.dev/" + "a" * 80
        msg = f"Subject line\n\nSee {long_url} for details.\n\nBug: 1\nTest: 1"
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_long_footer_lines_exempt(self) -> None:
        # Footers whose values are labels or commands are indexed by Gerrit
        # as single lines and must not be wrapped, so they are exempt.
        label = "//src/connectivity/wlan/drivers/lib/fidl-bridge/cpp/test/fidl"
        msg = (
            "Subject line\n\n"
            "Body line.\n\n"
            "Bug: 1\n"
            f"Test: fx bazel build --config=fuchsia_platform @{label}\n"
            f"Bazel-Migration-Target: {label}\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_long_no_ifttt_lines_exempt(self) -> None:
        # Gerrit's IfThisThenAnalyzer requires `NO_IFTTT=<reason>` (with '=')
        # and matches it anywhere in the commit message. Long justifications or
        # file paths should not trigger body line-length warnings.
        long_reason = (
            "NO_IFTTT=Visibility-only change in "
            "//build/config/rust/lints/BUILD.bazel that does not affect paired files."
        )
        self.assertGreater(len(long_reason), 72)

        for msg in (
            # In a body paragraph
            f"Subject line\n\n{long_reason}\n\nBug: 1\nTest: 1\n",
            # In the footer block
            f"Subject line\n\nBody line.\n\nBug: 1\nTest: 1\n{long_reason}\n",
        ):
            with self.subTest(msg=msg):
                findings = commit_msg_checker.check_commit_message(msg)
                self.assertEqual(findings, [])

    def test_long_no_ifttt_invalid_variants_not_exempt(self) -> None:
        # Only exact `NO_IFTTT=` at the start of a line is recognized by
        # Gerrit's IfThisThenAnalyzer; colon, lowercase, spaced '=', or
        # mid-line occurrences are not exempt from line wrapping.
        suffix = "a" * 80
        for line in (
            f"NO_IFTTT: {suffix}",
            f"no_ifttt={suffix}",
            f"NO_IFTTT = {suffix}",
            f"See NO_IFTTT={suffix}",
        ):
            with self.subTest(line=line):
                msg = f"Subject line\n\n{line}\n\nBug: 1\nTest: 1\n"
                findings = commit_msg_checker.check_commit_message(msg)
                self.assertEqual(len(findings), 1)
                self.assertIn(
                    "Line 3 exceeds 72 characters", findings[0]["message"]
                )

    def test_quoted_revert_long_line_exempt(self) -> None:
        msg = (
            'Revert "Subject"\n\n' "> " + "a" * 80 + "\n\n" "Bug: 1\n" "Test: 1"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_comment_long_line_exempt(self) -> None:
        msg = (
            "Subject line\n\n"
            "Body line.\n\n"
            "# " + "a" * 80 + "\n\n"
            "Bug: 1\n"
            "Test: 1\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_revert_message_quoted_change_id_allowed(self) -> None:
        msg = (
            'Revert "Some commit"\n\n'
            "This reverts commit 12345.\n\n"
            "> Change-Id: Ioldchangeid\n"
            "> Bug: 123\n\n"
            "Bug: 123\n"
            "Test: Reverting patch\n"
            "Change-Id: I1234567890123456789012345678901234567890\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_multiple_change_id_errors(self) -> None:
        msg = (
            "Subject line\n\n"
            "Bug: 123\n"
            "Test: Unit test\n"
            "Change-Id: I1111111111111111111111111111111111111111\n"
            "Change-Id: I2222222222222222222222222222222222222222\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertTrue(
            any(
                f["level"] == "error"
                and "Multiple 'Change-Id:' footers" in f["message"]
                for f in findings
            )
        )

    def test_agent_metadata_single_allowed(self) -> None:
        msg = (
            "Subject line\n\n"
            "Bug: 123\n"
            "Test: Unit test\n"
            "TAG: agy\n"
            "CONV: 6344ad16-d9ce-483d-9b98-affe2f24feac\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_agent_metadata_different_tags_allowed(self) -> None:
        msg = (
            "Subject line\n\n"
            "Bug: 123\n"
            "Test: Unit test\n"
            "TAG: agy\n"
            "TAG: refactor\n"
            "CONV: 6344ad16-first\n"
            "CONV: 6344ad16-second\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertEqual(findings, [])

    def test_agent_metadata_duplicate_warns(self) -> None:
        msg = (
            "Subject line\n\n"
            "Bug: 123\n"
            "Test: Unit test\n"
            "TAG=agy\n"
            "TAG: agy\n"
            "CONV=6344ad16-same\n"
            "CONV: 6344ad16-same\n"
        )
        findings = commit_msg_checker.check_commit_message(msg)
        self.assertTrue(
            any(
                "Duplicate 'TAG' metadata line" in f["message"]
                for f in findings
            )
        )
        self.assertTrue(
            any(
                "Duplicate 'CONV' metadata line" in f["message"]
                for f in findings
            )
        )


if __name__ == "__main__":
    unittest.main()
