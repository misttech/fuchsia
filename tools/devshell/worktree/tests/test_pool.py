# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

worktree_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, worktree_dir)

import argparse

from subcommands import lease as lease_cmd
from subcommands import pool_add as pool_add_cmd
from worktree import NoFreeWorktreesError, WorktreeState
from worktree_pool import ADJECTIVES, NOUNS, WorktreePool


class TestWorktreePool(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)
        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.pool = WorktreePool(fuchsia_dir=str(self.fuchsia_dir))

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_empty(self) -> None:
        self.assertEqual(self.pool.get_worktrees(), [])

    def test_invalid_state_transitions(self) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt_path}\n")

        wt = self.pool.get_worktree_by_name("wt1")
        self.assertEqual(wt.get_state(), WorktreeState.FREE)

        # Cannot release if FREE
        with self.assertRaises(RuntimeError):
            wt.release_lease()

        # Lease it
        wt.acquire_lease(task_id=None)
        self.assertEqual(wt.get_state(), WorktreeState.LEASED)

        # Cannot lease again
        with self.assertRaises(RuntimeError):
            wt.acquire_lease(task_id=None)

    def test_get_any_free_worktree(self) -> None:
        with self.assertRaises(NoFreeWorktreesError):
            self.pool.get_any_free_worktree()

        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt_path}\n")

        wt = self.pool.get_any_free_worktree()
        self.assertEqual(wt.name, "wt1")

    @patch("worktree.run_git")
    def test_release_detaches_head(self, mock_run_git: MagicMock) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt_path}\n")
        wt = self.pool.get_worktrees()[0]
        wt.acquire_lease("my-task")
        wt.release_lease()
        mock_run_git.assert_called_once_with(
            wt.path, ["checkout", "--detach"], quiet=True, check=True
        )

    def test_generate_random_pool_name(self) -> None:
        name = self.pool._generate_random_pool_name()
        self.assertIn("-", name)
        adj, noun = name.split("-", 1)
        self.assertIn(adj, ADJECTIVES)
        self.assertIn(noun, NOUNS)


class TestLeaseSubcommand(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)
        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.pool = WorktreePool(fuchsia_dir=str(self.fuchsia_dir))

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_lease_named(self) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt_path}\n")

        args = argparse.Namespace(
            name="wt1", any=False, sync=False, task_id=None, json=False
        )
        lease_cmd.run(args, self.pool)
        wt = self.pool.get_worktree_by_name("wt1")
        self.assertEqual(wt.get_state(), WorktreeState.LEASED)

    def test_lease_any(self) -> None:
        wt1_path = self.jiri_root / "worktrees" / "wt1"
        wt2_path = self.jiri_root / "worktrees" / "wt2"
        wt1_path.mkdir(parents=True, exist_ok=True)
        wt2_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt1_path}\n{wt2_path}\n")

        wt1 = self.pool.get_worktree_by_name("wt1")
        wt1.acquire_lease(None)

        args = argparse.Namespace(
            name=None, any=True, sync=False, task_id=None, json=False
        )
        lease_cmd.run(args, self.pool)
        self.assertEqual(wt1.get_state(), WorktreeState.LEASED)
        wt2 = self.pool.get_worktree_by_name("wt2")
        self.assertEqual(wt2.get_state(), WorktreeState.LEASED)

    def test_lease_mutually_exclusive_error(self) -> None:
        args = argparse.Namespace(
            name="wt1", any=True, sync=False, task_id=None, json=False
        )
        with self.assertRaises(SystemExit):
            lease_cmd.run(args, self.pool)

    def test_lease_missing_args_error(self) -> None:
        args = argparse.Namespace(
            name=None, any=False, sync=False, task_id=None, json=False
        )
        with self.assertRaises(SystemExit):
            lease_cmd.run(args, self.pool)

    def test_lease_any_no_free(self) -> None:
        wt1_path = self.jiri_root / "worktrees" / "wt1"
        wt1_path.mkdir(parents=True, exist_ok=True)
        with open(self.pool.registry_file, "w") as f:
            f.write(f"{wt1_path}\n")
        wt1 = self.pool.get_worktree_by_name("wt1")
        wt1.acquire_lease(None)

        args = argparse.Namespace(
            name=None, any=True, sync=False, task_id=None, json=False
        )
        with self.assertRaises(NoFreeWorktreesError):
            lease_cmd.run(args, self.pool)


class TestPoolAddSubcommand(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)
        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.pool = WorktreePool(fuchsia_dir=str(self.fuchsia_dir))
        self.patcher_jiri = patch("worktree_pool.run_jiri")
        self.patcher_fx = patch("subcommands.pool_add.run_fx")
        self.mock_jiri = self.patcher_jiri.start()
        self.mock_fx = self.patcher_fx.start()

    def tearDown(self) -> None:
        self.patcher_jiri.stop()
        self.patcher_fx.stop()
        self.temp_dir.cleanup()

    def test_add_multiple_set_args(self) -> None:
        wt_path = self.pool.worktrees_dir / "wt1"
        self.pool.registry_file.write_text(f"{wt_path}\n")
        args = argparse.Namespace(
            name="wt1",
            set=["core.x64 --out out/core", "workbench.arm64 --out out/wb"],
        )
        pool_add_cmd.run(args, self.pool)
        self.assertEqual(self.mock_fx.call_count, 2)
        wt = self.pool.get_worktree_by_name("wt1")
        self.assertEqual(wt.name, "wt1")


if __name__ == "__main__":
    unittest.main()
