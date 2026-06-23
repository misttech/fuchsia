# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import tempfile
import unittest
from io import StringIO
from pathlib import Path
from unittest.mock import MagicMock, patch

worktree_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, worktree_dir)

import argparse

from subcommands import add as add_cmd
from subcommands import pool_add as pool_add_cmd
from subcommands import pool_remove as pool_remove_cmd
from subcommands import remove as remove_cmd
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
        wt.acquire_lease(task_id="test")
        self.assertEqual(wt.get_state(), WorktreeState.LEASED)

        # Cannot lease again
        with self.assertRaises(RuntimeError):
            wt.acquire_lease(task_id="test2")

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


class TestActiveAddSubcommand(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)
        self.jiri_root = self.fuchsia_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.pool = WorktreePool(fuchsia_dir=str(self.fuchsia_dir))
        self.patcher_git = patch("subcommands.add.run_git")
        self.mock_git = self.patcher_git.start()

    def tearDown(self) -> None:
        self.patcher_git.stop()
        self.temp_dir.cleanup()

    def test_add_claims_slot(self) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        self.pool.registry_file.write_text(f"{wt_path}\n")

        args = argparse.Namespace(
            name="my-feat", pool_name=None, sync=False, json=False
        )
        with patch("sys.stdout", new_callable=StringIO) as mock_out:
            add_cmd.run(args, self.pool)
            self.assertIn(".jiri_root/worktrees/my-feat", mock_out.getvalue())
        wt = self.pool.get_worktrees()[0]
        self.assertEqual(wt.get_state(), WorktreeState.LEASED)
        lease = wt.get_lease_info()
        assert lease is not None
        self.assertEqual(lease.task_id, "my-feat")

    @patch("worktree.run_git")
    def test_remove_by_task_id(self, mock_run_git: MagicMock) -> None:
        wt_path = self.jiri_root / "worktrees" / "wt1"
        wt_path.mkdir(parents=True, exist_ok=True)
        self.pool.registry_file.write_text(f"{wt_path}\n")
        wt = self.pool.get_worktrees()[0]
        wt.acquire_lease("my-task-123")

        args = argparse.Namespace(name="my-task-123")
        remove_cmd.run(args, self.pool)
        self.assertEqual(wt.get_state(), WorktreeState.FREE)

        args_pool = argparse.Namespace(name="my-task-123", force=False)
        with self.assertRaises(KeyError):
            pool_remove_cmd.run(args_pool, self.pool)


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
        self.pool.get_worktree_by_name("wt1")


if __name__ == "__main__":
    unittest.main()
