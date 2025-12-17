# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from pathlib import Path
from typing import List
from unittest.mock import MagicMock, patch

from git import (
    ArgsCollection,
    Context,
    get_repo_root_for_repo,
    get_target_repository_at_path,
)


class TestGetRelativePathFromArgs(unittest.TestCase):
    def test_c_and_git_dir_raises(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/repo",
            "--",
            "-C",
            "foo",
            "--git-dir",
            "bar",
            "status",
        ]
        context = Context(ArgsCollection(args))
        with self.assertRaises(ValueError):
            context.get_relative_path()

    def test_c_absolute(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/repo",
            "--",
            "-C",
            "/repo/foo",
            "status",
        ]
        context = Context(ArgsCollection(args))
        self.assertEqual(context.get_relative_path(), "foo")

    def test_c_relative(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/cwd",
            "--",
            "-C",
            "foo",
            "status",
        ]
        context = Context(ArgsCollection(args))
        self.assertEqual(context.get_relative_path(), "foo")

    def test_git_dir_absolute(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/repo",
            "--",
            "--git-dir",
            "/repo/foo/.git",
            "status",
        ]
        context = Context(ArgsCollection(args))
        self.assertEqual(context.get_relative_path(), "foo")

    def test_git_dir_relative(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/cwd",
            "--",
            "--git-dir",
            "foo/.git",
            "status",
        ]
        context = Context(ArgsCollection(args))
        self.assertEqual(context.get_relative_path(), "foo")

    def test_git_dir_no_dot_git(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/cwd",
            "--repository-root",
            "/repo",
            "--",
            "--git-dir",
            "/repo/foo",
            "status",
        ]
        context = Context(ArgsCollection(args))
        with self.assertRaises(ValueError):
            context.get_relative_path()

    def test_invoker_cwd(self) -> None:
        args = [
            "--real-git",
            "git",
            "--invoker-cwd",
            "/repo/foo",
            "--repository-root",
            "/repo",
            "--",
            "status",
        ]
        context = Context(ArgsCollection(args))
        self.assertEqual(context.get_relative_path(), "foo")


class TestGetTargetRepositoryAtPath(unittest.TestCase):
    @patch("git.get_submodule_paths")
    def test_default_repo_root(
        self, mock_get_submodule_paths: MagicMock
    ) -> None:
        mock_get_submodule_paths.return_value = []
        repository_root = Path("/foo/fuchsia")
        self.assertEqual(get_target_repository_at_path("", repository_root), "")

    @patch("git.get_submodule_paths")
    def test_default_repo_root_with_subdirectory(
        self, mock_get_submodule_paths: MagicMock
    ) -> None:
        mock_get_submodule_paths.return_value = []
        repository_root = Path("/foo/fuchsia")
        self.assertEqual(
            get_target_repository_at_path("bar", repository_root), ""
        )

    @patch("git.get_submodule_paths")
    def test_repo_root_with_valid_submodule(
        self, mock_get_submodule_paths: MagicMock
    ) -> None:
        mock_get_submodule_paths.return_value = ["subdir"]
        repository_root = Path("/foo/fuchsia")
        self.assertEqual(
            get_target_repository_at_path("subdir/bar", repository_root),
            "subdir",
        )

    @patch("git.get_submodule_paths")
    def test_repo_root_with_invalid_submodule(
        self, mock_get_submodule_paths: MagicMock
    ) -> None:
        mock_get_submodule_paths.return_value = ["otherdir"]
        repository_root = Path("/foo/fuchsia")
        self.assertEqual(
            get_target_repository_at_path("subdir", repository_root), ""
        )

    @patch("git.get_submodule_paths")
    def test_repo_root_with_nested_submodules(
        self, mock_get_submodule_paths: MagicMock
    ) -> None:
        repository_root = Path("/foo/fuchsia")

        # Use side_effect to return different values based on input path
        def side_effect(path: Path) -> List[str]:
            if path == repository_root:
                return ["without_submodule"]
            elif path == repository_root / "with_submodule":
                return ["submodule"]
            return []

        mock_get_submodule_paths.side_effect = side_effect

        self.assertEqual(
            get_target_repository_at_path(
                "with_submodule/submodule/baz", repository_root
            ),
            "with_submodule/submodule",
        )


class TestGetRepoRootForRepo(unittest.TestCase):
    def test_main_repo(self) -> None:
        self.assertEqual(get_repo_root_for_repo(""), "fuchsia")

    def test_submodule(self) -> None:
        self.assertEqual(get_repo_root_for_repo("subdir"), "fuchsia/subdir")

    def test_nested_submodule(self) -> None:
        self.assertEqual(
            get_repo_root_for_repo("subdir/nested"), "fuchsia/subdir/nested"
        )


class TestArgsCollection(unittest.TestCase):
    def test_parse_simple_command(self) -> None:
        args = [
            "--real-git",
            "/usr/bin/git",
            "--invoker-cwd",
            "/tmp",
            "--",
            "status",
        ]
        collection = ArgsCollection(args)
        self.assertEqual(
            collection.polyfill_args,
            ["--real-git", "/usr/bin/git", "--invoker-cwd", "/tmp"],
        )
        self.assertEqual(collection.global_git_args, [])
        self.assertEqual(collection.command_name, "status")
        self.assertEqual(collection.remaining_args, [])

    def test_parse_command_with_args(self) -> None:
        args = [
            "--real-git",
            "/usr/bin/git",
            "--invoker-cwd",
            "/tmp",
            "--",
            "ls-files",
            "-z",
        ]
        collection = ArgsCollection(args)
        self.assertEqual(collection.command_name, "ls-files")
        self.assertEqual(collection.remaining_args, ["-z"])

    def test_parse_with_global_args(self) -> None:
        args = [
            "--real-git",
            "/usr/bin/git",
            "--invoker-cwd",
            "/tmp",
            "--",
            "-C",
            "/foo",
            "status",
        ]
        collection = ArgsCollection(args)
        self.assertEqual(collection.global_git_args, ["-C", "/foo"])
        self.assertEqual(collection.command_name, "status")

    def test_raises_error_without_separator(self) -> None:
        args = ["status"]
        with self.assertRaises(ValueError):
            ArgsCollection(args)

    def test_raises_error_without_command(self) -> None:
        args = ["--real-git", "/usr/bin/git", "--invoker-cwd", "/tmp", "--"]
        with self.assertRaises(ValueError):
            ArgsCollection(args)


if __name__ == "__main__":
    unittest.main()
