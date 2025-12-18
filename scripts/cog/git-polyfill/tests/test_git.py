<<<<<<< PATCH SET (6c1cb7dcd3067c1894052e66376aa75e9f2d050a Revert "Reland "[cog] refactor repository path logic"")
||||||| BASE      (b9499a02cd707b5d8b709c1eb8979ad440d70024 Reland "[cog] refactor repository path logic")
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
=======
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from pathlib import Path
from typing import List
from unittest.mock import MagicMock, call, patch

from git import (
    ArgsCollection,
    Context,
    RevParseCommand,
    get_repo_root_for_repo,
    get_target_repository_at_path,
)


class TestRevParseCommand(unittest.TestCase):
    def setUp(self) -> None:
        self.mock_context = MagicMock(spec=Context)
        self.mock_context.args = MagicMock()
        self.command = RevParseCommand()

    def test_show_toplevel(self) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.args.remaining_args = ["--show-toplevel"]

        # We need to mock get_relative_path because execute calls it
        self.mock_context.get_relative_path.return_value = ""

        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_called_with(str(Path("/repo")))

    @patch("git.get_target_repository_at_path")
    def test_show_toplevel_in_submodule(
        self, mock_get_target_repo: MagicMock
    ) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.args.remaining_args = ["--show-toplevel"]

        # We are deep inside a submodule
        self.mock_context.get_relative_path.return_value = "sub/module/dir"

        # The target repo detection logic says we are in 'sub/module'
        mock_get_target_repo.return_value = "sub/module"

        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_called_with(
            str(Path("/repo/sub/module"))
        )

    @patch("git.get_workspace_id_and_snapshot_version")
    def test_head_revision(self, mock_get_workspace_info: MagicMock) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.get_relative_path.return_value = ""
        self.mock_context.args.remaining_args = ["HEAD"]

        mock_get_workspace_info.return_value = ("workspace-id", 123)

        # Mock run_real_git response
        self.mock_context.run_real_git.return_value = (
            'result {\n  commit_hash: "abcdef123456"\n}'
        )

        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_called_with("abcdef123456")

    @patch("git.get_workspace_id_and_snapshot_version")
    def test_mixed_args_order(self, mock_get_workspace_info: MagicMock) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.get_relative_path.return_value = ""
        mock_get_workspace_info.return_value = ("workspace-id", 123)
        self.mock_context.run_real_git.return_value = (
            'result {\n  commit_hash: "abcdef123456"\n}'
        )

        # Test order: --show-toplevel, HEAD
        self.mock_context.args.remaining_args = ["--show-toplevel", "HEAD"]
        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_has_calls(
            [call(str(Path("/repo"))), call("abcdef123456")]
        )

        self.mock_context.print.reset_mock()

        # Test order: HEAD, --show-toplevel
        self.mock_context.args.remaining_args = ["HEAD", "--show-toplevel"]
        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_has_calls(
            [call("abcdef123456"), call(str(Path("/repo")))]
        )

    def test_unsupported_revision(self) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.get_relative_path.return_value = ""
        self.mock_context.args.remaining_args = ["main"]

        self.assertEqual(self.command.execute(self.mock_context), 1)
        self.mock_context.error.assert_called_with(
            "cog workspaces only support 'HEAD' revisions at this time"
        )

    def test_not_in_cog_workspace(self) -> None:
        self.mock_context.get_repository_root.return_value = None
        self.assertEqual(self.command.execute(self.mock_context), 1)
        self.mock_context.error.assert_called_with("Not in a cog workspace")


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
>>>>>>> BASE      (6b6ee4e85d2cfc7f0894f634b28b7d6b78d9696d [ffx][monitor] Add one second delay to background thread)
