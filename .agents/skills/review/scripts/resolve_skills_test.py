#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for resolve_skills.py.

Run: python3 -B .agents/skills/review/scripts/resolve_skills_test.py
"""

import contextlib
import io
import json
import os
import sys
import tempfile
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
configs: {
  id: "docs"
  skills: "docs/skills/edit-fuchsia-doc/SKILL.md"
  include_filters: {
    project: "fuchsia"
    project: "vendor/google"
    path_regex: "docs/.*\\.md"
  }
  exclude_filters: {
    path_regex: "docs/gen/.*"
  }
  exclude_filters: {
    project: "vendor/google"
  }
}
"""

VENDOR_CONFIG = r"""
configs: {
  id: "turquoise-code-review-agent"
  use_default_subagents: true
  include_filters: {
    project: "vendor/google"
    path_regex: ".*"
  }
}
configs: {
  id: "modem"
  skills: "iris/modem/.agents/skills/mtk-t900"
  include_filters: {
    project: "vendor/google"
    path_regex: "iris/modem/.*"
  }
}
configs: {
  id: "bare"
  skills: "vendor-skill"
  include_filters: {
    path_regex: ".*\\.json5"
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
        self.assertEqual(_resolve(["docs/README.yaml"]), {})

    def test_exclude_filters(self):
        # Like CRACS, a file matching any exclude_filters block is dropped.
        self.assertEqual(
            _resolve(["docs/README.md", "docs/gen/api.md"]),
            {"/repo/docs/skills/edit-fuchsia-doc/SKILL.md": ["docs/README.md"]},
        )

    def test_exclude_filter_without_path_regex_excludes_project(self):
        self.assertEqual(
            _resolve(["docs/README.md"], project="vendor/google"), {}
        )

    def test_skill_ending_in_skill_md_is_not_doubled(self):
        self.assertEqual(
            resolve_skills.skill_file("docs/skills/x/SKILL.md", ROOT),
            "/repo/docs/skills/x/SKILL.md",
        )

    def test_extra_configs_resolve_from_their_own_root(self):
        out = {
            r["skill_path"]: r["files"]
            for r in resolve_skills.resolve(
                CONFIG,
                ["iris/modem/a.rs", "x/TESTING.json5"],
                ROOT,
                project="vendor/google",
                extra_configs=[(VENDOR_CONFIG, "/repo/vendor/google")],
            )
        }
        self.assertEqual(
            out,
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "iris/modem/a.rs"
                ],
                "/repo/vendor/google/iris/modem/.agents/skills/mtk-t900/"
                "SKILL.md": ["iris/modem/a.rs"],
                "/repo/vendor/google/.agents/skills/vendor-skill/SKILL.md": [
                    "x/TESTING.json5"
                ],
            },
        )

    def _write_tree(self, root, vendor=True):
        files = {resolve_skills.CONFIG: CONFIG}
        if vendor:
            files[
                os.path.join("vendor", "google", resolve_skills.CONFIG)
            ] = VENDOR_CONFIG
        for rel, text in files.items():
            path = os.path.join(root, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)

    def _main(self, root, files, project):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(
            io.StringIO()
        ):
            resolve_skills.main(
                [
                    f"--repo_root={root}",
                    f"--files={files}",
                    f"--project={project}",
                ]
            )
        return {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in json.loads(out.getvalue())
        }

    def test_main_loads_vendor_config_for_vendor_project(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            self.assertEqual(
                self._main(root, "iris/modem/a.rs", "vendor/google"),
                {
                    ".agents/skills/rust_best_practices/SKILL.md": [
                        "iris/modem/a.rs"
                    ],
                    "vendor/google/iris/modem/.agents/skills/mtk-t900/"
                    "SKILL.md": ["iris/modem/a.rs"],
                },
            )

    def test_main_ignores_vendor_config_for_fuchsia_project(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            self.assertEqual(self._main(root, "x/TESTING.json5", "fuchsia"), {})

    def test_main_without_vendor_config(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root, vendor=False)
            self.assertEqual(
                self._main(root, "iris/modem/a.rs", "vendor/google"),
                {
                    ".agents/skills/rust_best_practices/SKILL.md": [
                        "iris/modem/a.rs"
                    ]
                },
            )

    def test_repo_root_skips_vendor_google(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            start = os.path.join(root, "vendor", "google", "iris")
            os.makedirs(start)
            self.assertEqual(
                os.path.realpath(resolve_skills._repo_root(start)),
                os.path.realpath(root),
            )

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
