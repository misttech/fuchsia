# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

from git import (
    ArgsCollection,
    Context,
    LsFilesCommand,
    RevParseCommand,
    StatusCommand,
    get_repo_root_for_repo,
    get_submodule_paths,
    get_target_repository_at_path,
    verify_repository_root_is_cog,
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

    @patch("git.get_workspace_id_and_snapshot_version")
    def test_head_revision_not_found(
        self, mock_get_workspace_info: MagicMock
    ) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.get_relative_path.return_value = ""
        self.mock_context.args.remaining_args = ["HEAD"]
        mock_get_workspace_info.return_value = ("workspace-id", 123)

        # Mock run_real_git response with no hash
        self.mock_context.run_real_git.return_value = (
            'result {\n  some_other_field: "value"\n}'
        )

        # Expect return code 1
        self.assertEqual(self.command.execute(self.mock_context), 1)

        # Expect an error message
        self.mock_context.error.assert_called()

    @patch("git.get_workspace_id_and_snapshot_version")
    def test_cannot_determine_workspace_id(
        self, mock_get_workspace_info: MagicMock
    ) -> None:
        self.mock_context.get_repository_root.return_value = Path("/repo")
        self.mock_context.get_relative_path.return_value = ""
        self.mock_context.args.remaining_args = ["HEAD"]
        mock_get_workspace_info.return_value = ("", 0)
        self.assertEqual(self.command.execute(self.mock_context), 1)
        self.mock_context.error.assert_called_with(
            "Cannot determine workspace id or snapshot version"
        )


class TestStatusCommand(unittest.TestCase):
    def setUp(self) -> None:
        self.mock_context = MagicMock(spec=Context)
        self.mock_context.args = MagicMock()
        self.mock_context.args.global_git_args = []
        self.mock_context.git_subcommand_args = MagicMock()
        self.command = StatusCommand()

    def test_execution(self) -> None:
        self.assertEqual(self.command.execute(self.mock_context), 0)
        self.mock_context.print.assert_called_with("not implemented yet")


class TestLsFilesCommand(unittest.TestCase):
    def setUp(self) -> None:
        self.mock_context = MagicMock(spec=Context)
        self.mock_context.args = MagicMock()
        self.mock_context.args.global_git_args = []
        self.mock_context.args.remaining_args = []
        self.mock_context.invoker_cwd = "/cwd"
        self.command = LsFilesCommand()

    def test_basic_execution(self) -> None:
        self.command.run(self.mock_context)
        self.mock_context.run_real_git.assert_called_with(
            ["ls-files"], cwd="/cwd"
        )

    def test_with_arguments(self) -> None:
        self.mock_context.args.remaining_args = ["-z", "-c", "file.txt"]
        self.command.run(self.mock_context)
        self.mock_context.run_real_git.assert_called_with(
            ["ls-files", "-z", "-c", "file.txt"], cwd="/cwd"
        )

    def test_with_global_args(self) -> None:
        self.mock_context.args.global_git_args = ["-C", "/foo"]
        self.command.run(self.mock_context)
        self.mock_context.run_real_git.assert_called_with(
            ["-C", "/foo", "ls-files"], cwd="/cwd"
        )

    def test_output_formatting_z(self) -> None:
        self.mock_context.args.remaining_args = ["-z"]
        self.mock_context.run_real_git.return_value = "file1\0file2\0"
        self.command.run(self.mock_context)
        self.mock_context.output.assert_called_with("file1\0file2\0", end="\0")

    def test_output_formatting_newline(self) -> None:
        self.mock_context.args.remaining_args = []
        self.mock_context.run_real_git.return_value = "file1\nfile2\n"
        self.command.run(self.mock_context)
        self.mock_context.output.assert_called_with("file1\nfile2\n", end="\n")


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


class TestGetRepoRootForRepo(unittest.TestCase):
    def test_empty_repo(self) -> None:
        self.assertEqual(get_repo_root_for_repo(""), "fuchsia")

    def test_submodule_repo(self) -> None:
        self.assertEqual(get_repo_root_for_repo("foo/bar"), "fuchsia/foo/bar")


class TestGetSubmodulePaths(unittest.TestCase):
    def test_no_gitmodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            self.assertEqual(get_submodule_paths(tmp_dir), [])

    def test_with_nested_submodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)

            # Top-level .gitmodules
            gitmodules = tmp_dir / ".gitmodules"
            gitmodules.write_text('[submodule "foo"]\n\tpath = foo\n')

            # Nested submodule .gitmodules
            foo_dir = tmp_dir / "foo"
            foo_dir.mkdir()
            foo_gitmodules = foo_dir / ".gitmodules"
            foo_gitmodules.write_text('[submodule "bar"]\n\tpath = bar\n')

            self.assertEqual(get_submodule_paths(tmp_dir), ["foo"])
            self.assertEqual(get_submodule_paths(foo_dir), ["bar"])

    def test_with_submodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            gitmodules = tmp_dir / ".gitmodules"
            gitmodules.write_text(
                '[submodule "submodule1"]\n\tpath = path/to/sub1\n'
                '[submodule "submodule2"]\n\tpath = path/to/sub2\n'
            )

            self.assertEqual(
                get_submodule_paths(tmp_dir), ["path/to/sub1", "path/to/sub2"]
            )

    def test_malformed_gitmodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            gitmodules = tmp_dir / ".gitmodules"
            gitmodules.write_text("not a valid ini file")

            self.assertEqual(get_submodule_paths(tmp_dir), [])


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

    def test_repo_root_with_nested_submodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            repository_root = Path(tmp_dir_str)

            (repository_root / ".gitmodules").write_text(
                '[submodule "with_submodule"]\n\tpath = with_submodule\n'
                '[submodule "without_submodule"]\n\tpath = without_submodule\n'
            )
            (repository_root / "with_submodule").mkdir()

            (repository_root / "with_submodule" / ".gitmodules").write_text(
                '[submodule "submodule"]\n\tpath = submodule\n'
            )
            (repository_root / "with_submodule" / "submodule").mkdir()

            self.assertEqual(
                get_target_repository_at_path(
                    "with_submodule/submodule/baz", repository_root
                ),
                "with_submodule/submodule",
            )

    def test_repo_root_with_deeply_nested_submodules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            repository_root = Path(tmp_dir_str)

            # Level 1
            (repository_root / ".gitmodules").write_text(
                '[submodule "level1"]\n\tpath = level1\n'
            )
            (repository_root / "level1").mkdir()

            # Level 2
            (repository_root / "level1" / ".gitmodules").write_text(
                '[submodule "level2"]\n\tpath = level2\n'
            )
            (repository_root / "level1" / "level2").mkdir()

            # Level 3
            (repository_root / "level1" / "level2" / ".gitmodules").write_text(
                '[submodule "level3"]\n\tpath = level3\n'
            )
            (repository_root / "level1" / "level2" / "level3").mkdir()

            self.assertEqual(
                get_target_repository_at_path(
                    "level1/level2/level3/foo", repository_root
                ),
                "level1/level2/level3",
            )


class TestVerifyRepositoryRootIsCog(unittest.TestCase):
    def test_is_cog(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            repo_root = tmp_dir / "fuchsia"
            repo_root.mkdir()
            citc_dir = tmp_dir / ".citc"
            citc_dir.mkdir()

            self.assertTrue(verify_repository_root_is_cog(repo_root))

    def test_is_not_cog(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            repo_root = tmp_dir / "fuchsia"
            repo_root.mkdir()

            self.assertFalse(verify_repository_root_is_cog(repo_root))


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
