#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for render_report.py.

Run: python3 -B .agents/skills/review/scripts/render_report_test.py
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import render_report  # pylint: disable=wrong-import-position


class RenderReportTest(unittest.TestCase):
    def test_full_report(self):
        payload = {
            "decision": "CHANGES_REQUESTED",
            "summary": "Found one blocking issue.",
            "scope": "Working copy",
            "agent_skills": ["rust_best_practices", "general"],
            "findings": [
                {
                    "severity": "suggestion",
                    "comment": "Document panics.",
                    "rule_reference": "rust_best_practices/docs",
                    "rule_link": "file:///repo/.agents/skills/r/SKILL.md#L1-L2",
                    "file_path": "src/lib.rs",
                    "line_start": 46,
                },
                {
                    "severity": "blocking",
                    "comment": "Missing tests.",
                    "rule_reference": "general/tests",
                },
                {
                    "severity": "blocking",
                    "comment": "unwrap() on input.",
                    "file_path": "src/lib.rs",
                    "line_start": 47,
                    "line_end": 48,
                    "code_snippet": "fn f() {\n  x.unwrap()\n}",
                    "suggested_diff": "-a\n+b",
                },
            ],
        }
        expected = """\
## Code Review

❌ **Changes requested** — See inline comments for details.

Found one blocking issue.

- **Scope**: Working copy
- **Skills Evaluated**: `rust_best_practices`, `general`

---

- **#1**

  **🚨 Blocking** (*general/tests*): Missing tests.

---

### [`src/lib.rs`](file:///repo/src/lib.rs)

- **#2** [**L46**](file:///repo/src/lib.rs#L46)

  **💡 Suggestion** ([**rust_best_practices/docs**](file:///repo/.agents/skills/r/SKILL.md#L1-L2)): Document panics.

- **#3** [**L47-48** `fn f() { ...`](file:///repo/src/lib.rs#L47-L48)

  **🚨 Blocking**: unwrap() on input.

  ```diff
  -a
  +b
  ```
"""
        self.assertEqual(render_report.render(payload, "/repo"), expected)
        self.assertEqual(
            [f["finding_id"] for f in payload["findings"]], ["1", "2", "3"]
        )

    def test_approved_without_findings(self):
        self.assertEqual(
            render_report.render({"decision": "approved"}),
            "## Code Review\n\n✅ **Approved** — All checks pass.\n",
        )

    def test_invalid_decision(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render({"decision": "lgtm"})

    def test_invalid_severity(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [{"severity": "nit", "comment": "x"}],
                }
            )

    def test_empty_comment(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [{"severity": "blocking", "comment": " "}],
                }
            )

    def test_bad_line_range(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [
                        {
                            "severity": "blocking",
                            "comment": "x",
                            "file_path": "a",
                            "line_start": 5,
                            "line_end": 4,
                        }
                    ],
                }
            )


if __name__ == "__main__":
    unittest.main()
