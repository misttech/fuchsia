#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for resolve_skills.py.

Run: python3 -B .agents/skills/review/scripts/resolve_skills_test.py
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import resolve_skills  # pylint: disable=wrong-import-position

ROOT = "/repo"
CONFIG = r"""
# comment
configs: {
  id: "default"
  use_default_subagents: true
  include_filters: {
    project: "fuchsia"
    path_regex: ".*"
  }
}
configs: {
  id: "rust"
  skills: "rust_best_practices"
  include_filters: {
    project: "fuchsia"
    project: "vendor/google"
    path_regex: ".*\\.rs"
  }
}
configs: {
  id: "drivers"
  skills: "src/devices/skills/driver_file_structure"
  skills: "src/devices/skills/fuchsia-suspendable"
  include_filters: {
    project: "fuchsia"
    path_regex: "(.*/)?drivers/.*\\.(rs|cml)"
  }
}
configs: {
  id: "starnix"
  skills: "starnix-guidelines"
  include_filters: {
    path_regex: "(.*/)?starnix/.*"
  }
}
configs: {
  id: "lacewing"
  skills: "src/testing/end_to_end/.agents/skills/lacewing"
  include_filters: {
    project: "fuchsia"
    path_regex: "tests/end_to_end/.*\\.py"
  }
}
"""


def _resolve(files, project="fuchsia"):
    return {
        r["skill_path"]: r["files"]
        for r in resolve_skills.resolve(CONFIG, files, ROOT, project=project)
    }


class ResolveSkillsTest(unittest.TestCase):
    def test_bare_skill_resolves_under_agents_skills(self):
        self.assertEqual(
            _resolve(["src/lib/foo.rs"]),
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "src/lib/foo.rs"
                ]
            },
        )

    def test_path_skill_resolves_from_repo_root_and_groups_files(self):
        out = _resolve(
            ["src/devices/x/drivers/a.rs", "src/devices/x/drivers/meta/a.cml"]
        )
        self.assertEqual(
            out["/repo/src/devices/skills/fuchsia-suspendable/SKILL.md"],
            ["src/devices/x/drivers/a.rs", "src/devices/x/drivers/meta/a.cml"],
        )
        self.assertEqual(
            out["/repo/.agents/skills/rust_best_practices/SKILL.md"],
            ["src/devices/x/drivers/a.rs"],
        )

    def test_full_match_not_search(self):
        # Like CRACS, path_regex must match the whole path.
        self.assertEqual(_resolve(["src/tests/end_to_end/foo.py"]), {})

    def test_entry_without_project_applies_to_any_project(self):
        out = _resolve(["starnix/kernel/a.rs"], project="vendor/google")
        self.assertIn("/repo/.agents/skills/starnix-guidelines/SKILL.md", out)

    def test_project_filter(self):
        self.assertEqual(
            _resolve(["a/drivers/b.rs"], project="vendor/google"),
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "a/drivers/b.rs"
                ]
            },
        )

    def test_entries_without_skills_add_nothing(self):
        self.assertEqual(_resolve(["docs/README.md"]), {})

    def test_real_config_parses(self):
        root = os.path.abspath(
            os.path.join(os.path.dirname(__file__), "..", "..", "..", "..")
        )
        path = os.path.join(root, resolve_skills.CONFIG)
        if not os.path.isfile(path):
            self.skipTest("agent_configs.textproto not checked out")
        with open(path, encoding="utf-8") as f:
            configs = resolve_skills.parse_textproto(f.read())["configs"]
        self.assertTrue(all("id" in c for c in configs))


if __name__ == "__main__":
    unittest.main()
