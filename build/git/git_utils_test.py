# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for git_utils.py."""

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from build.git import git_utils


class GitUtilsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir_obj = tempfile.TemporaryDirectory()
        self.repo_dir = Path(self.temp_dir_obj.name)

    def tearDown(self) -> None:
        self.temp_dir_obj.cleanup()

    @mock.patch.object(subprocess, "run")
    def test_get_git_path_relative(self, mock_run: mock.Mock) -> None:
        mock_run.return_value = mock.Mock(returncode=0, stdout=".git/HEAD\n")
        result = git_utils.get_git_path(self.repo_dir, "HEAD")
        self.assertEqual(result, (self.repo_dir / ".git/HEAD").resolve())
        mock_run.assert_called_once_with(
            [
                "git",
                "--no-optional-locks",
                "-C",
                str(self.repo_dir),
                "rev-parse",
                "--git-path",
                "HEAD",
            ],
            capture_output=True,
            text=True,
            check=True,
        )

    @mock.patch.object(subprocess, "run")
    def test_get_git_path_absolute(self, mock_run: mock.Mock) -> None:
        abs_path = (self.repo_dir / "custom" / "HEAD").resolve()
        mock_run.return_value = mock.Mock(returncode=0, stdout=f"{abs_path}\n")
        result = git_utils.get_git_path(self.repo_dir, "HEAD")
        self.assertEqual(result, abs_path)

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(git_utils, "get_git_path")
    def test_get_git_ref_detached(
        self, mock_get_git_path: mock.Mock, mock_run: mock.Mock
    ) -> None:
        mock_run.return_value = mock.Mock(returncode=1, stdout="")
        head_path = self.repo_dir / ".git" / "HEAD"
        mock_get_git_path.return_value = head_path

        result = git_utils.get_git_ref(self.repo_dir)
        self.assertEqual(result, head_path)
        mock_get_git_path.assert_called_once_with(self.repo_dir, "HEAD")

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(git_utils, "get_git_path")
    def test_get_git_ref_loose_exists(
        self, mock_get_git_path: mock.Mock, mock_run: mock.Mock
    ) -> None:
        mock_run.return_value = mock.Mock(
            returncode=0, stdout="refs/heads/main\n"
        )
        ref_file = self.repo_dir / ".git" / "refs" / "heads" / "main"
        ref_file.parent.mkdir(parents=True, exist_ok=True)
        ref_file.write_text("1234567890abcdef1234567890abcdef12345678\n")
        mock_get_git_path.return_value = ref_file

        result = git_utils.get_git_ref(self.repo_dir)
        self.assertEqual(result, ref_file)

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(git_utils, "get_git_path")
    def test_get_git_ref_packed_fallback(
        self, mock_get_git_path: mock.Mock, mock_run: mock.Mock
    ) -> None:
        mock_run.return_value = mock.Mock(
            returncode=0, stdout="refs/heads/main\n"
        )
        loose_ref = self.repo_dir / ".git" / "refs" / "heads" / "nonexistent"
        packed_refs = self.repo_dir / ".git" / "packed-refs"
        packed_refs.parent.mkdir(parents=True, exist_ok=True)
        packed_refs.write_text("# pack-refs with: peeled fully-peeled sorted\n")

        def side_effect(repo_dir: Path, name: str) -> Path:
            if name == "packed-refs":
                return packed_refs
            return loose_ref

        mock_get_git_path.side_effect = side_effect

        result = git_utils.get_git_ref(self.repo_dir)
        self.assertEqual(result, packed_refs)

    @mock.patch.object(git_utils, "get_git_ref")
    @mock.patch.object(git_utils, "get_git_path")
    def test_find_git_head_inputs(
        self, mock_get_git_path: mock.Mock, mock_get_git_ref: mock.Mock
    ) -> None:
        git_dir = self.repo_dir / ".git"
        git_dir.mkdir(parents=True)
        head_path = git_dir / "HEAD"
        head_path.write_text("ref: refs/heads/main\n")
        config_path = git_dir / "config"
        config_path.write_text("[core]\n")
        ref_path = git_dir / "refs" / "heads" / "main"
        index_path = git_dir / "index"
        index_path.write_text("index content")

        def mock_path_lookup(repo_dir: Path, name: str) -> Path:
            if name == "HEAD":
                return head_path
            if name == "config":
                return config_path
            if name == "packed-refs":
                return git_dir / "packed-refs"
            if name == "reftable":
                return git_dir / "reftable"
            if name == "index":
                return index_path
            return repo_dir / name

        mock_get_git_path.side_effect = mock_path_lookup
        mock_get_git_ref.return_value = ref_path

        inputs = git_utils.find_git_head_inputs(self.repo_dir, track_index=True)
        self.assertIn(head_path, inputs)
        self.assertIn(config_path, inputs)
        self.assertIn(ref_path, inputs)
        self.assertIn(index_path, inputs)

    @mock.patch.object(subprocess, "run")
    def test_get_best_ref(self, mock_run: mock.Mock) -> None:
        mock_run.side_effect = [
            mock.Mock(returncode=1),
            mock.Mock(returncode=0),
        ]
        best_ref = git_utils._get_best_ref(self.repo_dir)
        self.assertEqual(best_ref, "jiri/head")

    @mock.patch.object(subprocess, "run")
    @mock.patch.object(git_utils, "_get_best_ref", return_value="HEAD")
    def test_get_git_revision(
        self, mock_best_ref: mock.Mock, mock_run: mock.Mock
    ) -> None:
        mock_run.return_value = mock.Mock(
            returncode=0,
            stdout="0123456789abcdef0123456789abcdef01234567\n",
        )
        rev = git_utils.get_git_revision(self.repo_dir)
        self.assertEqual(rev, "0123456789abcdef0123456789abcdef01234567")


if __name__ == "__main__":
    unittest.main()
