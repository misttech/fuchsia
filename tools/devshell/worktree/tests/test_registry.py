# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import tempfile
import unittest
from pathlib import Path

worktree_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, worktree_dir)

from worktree import WorktreeState
from worktree_registry import WorktreeRegistry


class TestWorktreeRegistry(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)
        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.registry = WorktreeRegistry(fuchsia_dir=str(self.fuchsia_dir))

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_empty(self) -> None:
        self.assertEqual(self.registry.get_worktrees(), [])

    def test_invalid_state_transitions(self) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        with open(self.registry.registry_file, "w") as f:
            f.write(f"{wt_path}\n")

        wt = self.registry.get_worktree_by_name("wt1")
        self.assertEqual(wt.get_state(), WorktreeState.FREE)

        # Cannot release if FREE
        with self.assertRaises(RuntimeError):
            wt.release_lease()

        # Lease it
        wt.acquire_lease(agent_id=None)
        self.assertEqual(wt.get_state(), WorktreeState.LEASED)

        # Cannot lease again
        with self.assertRaises(RuntimeError):
            wt.acquire_lease(agent_id=None)


if __name__ == "__main__":
    unittest.main()
