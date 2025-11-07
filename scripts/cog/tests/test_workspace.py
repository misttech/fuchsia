# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for workspace."""

import os
import unittest
from unittest.mock import MagicMock, patch

import cartfs
import mock_fs
import workspace
from parameterized import parameterized


class WorkspaceTest(unittest.TestCase):
    """Tests for Workspace."""

    def test_create_success(self) -> None:
        """Test that a Workspace instance can be created successfully."""
        with mock_fs.FileSystemTestHelper() as fs:
            fs.mkdir("testuser/test-workspace/fuchsia", mock_fs.FSType.COG)
            # Mock the environment variables and current working directory.
            with patch.dict(os.environ, {"USER": "testuser"}), patch.object(
                os,
                "getcwd",
                return_value=fs.full_path(
                    "testuser/test-workspace/fuchsia", mock_fs.FSType.COG
                ),
            ), patch.object(
                cartfs.Cartfs, "create", return_value=MagicMock()
            ) as mock_cartfs_create:
                ws = workspace.Workspace.create(cog_mount_point=fs.cog_dir)
                self.assertEqual(
                    ws.workspace_dir,
                    os.path.join(fs.cog_dir, "testuser/test-workspace"),
                )
                self.assertEqual(ws.repo_name, "fuchsia")
                self.assertEqual(ws.workspace_name, "test-workspace")
                mock_cartfs_create.assert_called_once()

    def test_create_user_not_found(self) -> None:
        """Test that UserNotFoundError is raised when the user is not found."""
        with mock_fs.FileSystemTestHelper():
            with patch.dict(os.environ, {"USER": ""}), self.assertRaises(
                workspace.UserNotFoundError
            ):
                workspace.Workspace.create()

    def test_create_not_in_cog_workspace(self) -> None:
        """Test that NotInCogWorkspaceError is raised when not in a Cog workspace."""
        with mock_fs.FileSystemTestHelper() as fs:
            fs.mkdir("some/other/dir", mock_fs.FSType.COG)
            with patch.dict(os.environ, {"USER": "testuser"}), patch.object(
                os,
                "getcwd",
                return_value=fs.full_path("some/other/dir", mock_fs.FSType.COG),
            ), self.assertRaises(workspace.NotInCogWorkspaceError):
                workspace.Workspace.create(cog_mount_point=fs.cog_dir)

    def test_create_cannot_find_repo_name(self) -> None:
        """Test that CannotFindRepoNameError is raised when the repo name cannot be found."""
        with mock_fs.FileSystemTestHelper() as fs:
            fs.mkdir("testuser/test-workspace", mock_fs.FSType.COG)
            with patch.dict(os.environ, {"USER": "testuser"}), patch.object(
                os,
                "getcwd",
                return_value=fs.full_path(
                    "testuser/test-workspace", mock_fs.FSType.COG
                ),
            ), patch.object(
                cartfs.Cartfs, "create", return_value=MagicMock()
            ), self.assertRaises(
                workspace.CannotFindRepoNameError
            ):
                workspace.Workspace.create(cog_mount_point=fs.cog_dir)

    def test_create_cartfs_error(self) -> None:
        """Test that CartfsError is raised when cartfs is not available."""
        with mock_fs.FileSystemTestHelper() as fs:
            fs.mkdir("testuser/test-workspace/fuchsia", mock_fs.FSType.COG)
            with patch.dict(os.environ, {"USER": "testuser"}), patch.object(
                os,
                "getcwd",
                return_value=fs.full_path(
                    "testuser/test-workspace/fuchsia", mock_fs.FSType.COG
                ),
            ), patch.object(
                cartfs.Cartfs, "create", side_effect=cartfs.CartfsError
            ), self.assertRaises(
                cartfs.CartfsError
            ):
                workspace.Workspace.create(cog_mount_point=fs.cog_dir)

    @parameterized.expand(
        [
            (
                "testuser/test-workspace/fuchsia",
                "testuser/test-workspace",
            ),
            (
                "testuser/test-workspace",
                "testuser/test-workspace",
            ),
        ]
    )
    def test_find_cog_workspace_directory_success(
        self, start_dir_suffix: str, expected_dir_suffix: str
    ) -> None:
        """Test that the workspace directory is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            start_dir = fs.full_path(start_dir_suffix, mock_fs.FSType.COG)
            expected_dir = fs.full_path(expected_dir_suffix, mock_fs.FSType.COG)
            fs.mkdir(start_dir_suffix, mock_fs.FSType.COG)
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir, "testuser", cog_mount_point=fs.cog_dir
            )
            self.assertEqual(actual_dir, expected_dir)

    def test_find_cog_workspace_directory_not_found(self) -> None:
        """Test that None is returned when not in a Cog workspace."""
        with mock_fs.FileSystemTestHelper() as fs:
            start_dir = fs.full_path("some/other/dir", mock_fs.FSType.COG)
            fs.mkdir("some/other/dir", mock_fs.FSType.COG)
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir, "testuser", cog_mount_point=fs.cog_dir
            )
            self.assertIsNone(actual_dir)

    def test_find_cog_workspace_directory_at_root(self) -> None:
        """Test that None is returned when starting at the root directory."""
        with mock_fs.FileSystemTestHelper() as fs:
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                fs.cog_dir, "testuser", cog_mount_point=fs.cog_dir
            )
            self.assertIsNone(actual_dir)

    def test_find_cog_workspace_escapes_user(self) -> None:
        """Test that the workspace directory is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            start_dir = fs.full_path(
                "test.user/test-workspace", mock_fs.FSType.COG
            )
            expected_dir = fs.full_path(
                "test.user/test-workspace", mock_fs.FSType.COG
            )
            fs.mkdir("test.user/test-workspace", mock_fs.FSType.COG)
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir, "test.user", cog_mount_point=fs.cog_dir
            )
            self.assertEqual(actual_dir, expected_dir)

    @parameterized.expand(
        [
            (
                "/google/cog/cloud/testuser/test-workspace",
                "/google/cog/cloud/testuser/test-workspace/fuchsia",
                "fuchsia",
            ),
            (
                "/google/cog/cloud/testuser/test-workspace",
                "/google/cog/cloud/testuser/test-workspace/fuchsia/out/default",
                "fuchsia",
            ),
        ]
    )
    def test_get_repo_name_from_path_success(
        self, workspace_dir: str, path: str, expected_repo_name: str
    ) -> None:
        """Test that the repo name is found correctly."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            workspace_dir, path
        )
        self.assertEqual(actual_repo_name, expected_repo_name)

    def test_get_repo_name_from_path_not_in_workspace(self) -> None:
        """Test that None is returned when the path is not in the workspace."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            "/google/cog/cloud/testuser/test-workspace", "/some/other/dir"
        )
        self.assertIsNone(actual_repo_name)

    def test_get_repo_name_from_path_same_as_workspace(self) -> None:
        """Test that None is returned when the path is the same as the workspace."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            "/google/cog/cloud/testuser/test-workspace",
            "/google/cog/cloud/testuser/test-workspace",
        )
        self.assertIsNone(actual_repo_name)

    def test_get_linked_cartfs_workspace_directory_success(self) -> None:
        """Test that the cartfs workspace directory is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )

            # A symlink points from cog to cartfs
            fs.symlink_from_cog_to_cartfs(
                os.path.join(
                    workspace_name, repo_name, workspace.CARTFS_SYMLINK_NAME
                ),
            )

            # A .repo-name file is created
            with open(
                os.path.join(
                    fs.cartfs_dir,
                    workspace.REPO_NAME_FILE_NAME,
                ),
                "w",
            ) as f:
                f.write(repo_name + "\n")  # keep the newline for the test

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG), repo_name
                )
            )
            self.assertEqual(actual_dir, fs.cartfs_dir)

    def test_get_linked_cartfs_workspace_directory_no_symlink_fails(
        self,
    ) -> None:
        """Test that None is returned when the symlink from cog to cartfs does not exist."""
        with mock_fs.FileSystemTestHelper() as fs:
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG), repo_name
                )
            )
            self.assertIsNone(actual_dir)

    def test_get_linked_cartfs_workspace_directory_symlink_dir_does_not_exist_fails(
        self,
    ) -> None:
        """Test that None is returned when the symlink directory does not exist."""
        with mock_fs.FileSystemTestHelper() as fs:
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )

            # A symlink points from cog to cartfs
            fs.symlink_from_cog_to_cartfs(
                os.path.join(
                    workspace_name, repo_name, workspace.CARTFS_SYMLINK_NAME
                ),
            )
            os.removedirs(fs.cartfs_dir)

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG), repo_name
                )
            )
            self.assertIsNone(actual_dir)

    def test_snapshot_from_previous_instance_success(
        self,
    ) -> None:
        """Test that snapshotting from a previous instance is successful."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir
            suggested_directory_name = "new_cartfs_dir"
            cartfs_instance.suggest_cartfs_directory_name.return_value = (
                suggested_directory_name
            )

            def mock_snapshot_workspace(
                workspace_to_snapshot_from: str,
                workspace_to_snapshot_to: str,
                cartfs_mount_point: str,
            ) -> None:
                os.mkdir(
                    os.path.join(cartfs_mount_point, suggested_directory_name)
                )

            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )
            with patch.object(
                ws,
                "_find_previous_instance",
                return_value="foo",
            ):
                result = ws.snapshot_from_previous_instance(
                    snapshot_function=mock_snapshot_workspace,
                )
                self.assertEqual(
                    result,
                    os.path.join(
                        cartfs_instance.mount_point, suggested_directory_name
                    ),
                )
                self.assertTrue(
                    os.path.isdir(
                        fs.full_path(
                            suggested_directory_name, mock_fs.FSType.CARTFS
                        )
                    )
                )

    def test_snapshot_from_previous_instance_no_previous_instance(
        self,
    ) -> None:
        """Test that None is returned when no previous instance is found."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )
            with patch.object(ws, "_find_previous_instance", return_value=None):
                result = ws.snapshot_from_previous_instance()
                self.assertIsNone(result)

    def test_snapshot_from_previous_instance_snapshot_error(self) -> None:
        """Test that None is returned when snapshotting raises a ValueError."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )
            with patch.object(
                ws,
                "_find_previous_instance",
                return_value=os.path.join(fs.cartfs_dir, "previous_instance"),
            ):

                def mock_snapshot_workspace(
                    workspace_to_snapshot_from: str,
                    workspace_to_snapshot_to: str,
                    cartfs_mount_point: str,
                ) -> None:
                    raise ValueError("test error")

                result = ws.snapshot_from_previous_instance(
                    snapshot_function=mock_snapshot_workspace
                )
                self.assertIsNone(result)

    def test_create_empty_cartfs_workspace_directory(self) -> None:
        """Test that an empty cartfs workspace directory is created."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir
            suggested_directory_name = "new_cartfs_dir"
            cartfs_instance.suggest_cartfs_directory_name.return_value = (
                suggested_directory_name
            )
            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )

            result = ws.create_empty_cartfs_workspace_directory()

            expected_dir = os.path.join(
                cartfs_instance.mount_point, suggested_directory_name
            )
            self.assertEqual(result, expected_dir)
            self.assertTrue(os.path.isdir(expected_dir))

            # Ensure that we write the name of the repository in cartfs
            self.assertEqual(
                fs.read(
                    f"{suggested_directory_name}/{workspace.REPO_NAME_FILE_NAME}",
                    mock_fs.FSType.CARTFS,
                ),
                "fuchsia",
            )

    def test_link_to_cartfs(self) -> None:
        """Test that the workspace can be linked to a cartfs directory."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            workspace_name = "test-workspace"
            workspace_dir = fs.full_path(workspace_name, mock_fs.FSType.COG)
            repo_name = "fuchsia"
            repo_dir = fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )

            ws = workspace.Workspace(
                workspace_dir=workspace_dir,
                repo_name=repo_name,
                workspace_name=workspace_name,
                cartfs_workspace_dir=None,
                cartfs_instance=cartfs_instance,
            )

            cartfs_workspace_dir = fs.mkdir(
                "cartfs_workspace_dir", mock_fs.FSType.CARTFS
            )
            ws.link_to_cartfs(cartfs_workspace_dir)

            symlink_path = os.path.join(repo_dir, workspace.CARTFS_SYMLINK_NAME)
            self.assertTrue(os.path.islink(symlink_path))
            self.assertEqual(os.readlink(symlink_path), cartfs_workspace_dir)

    def test_find_previous_instance_success(self) -> None:
        """Test that the previous instance is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            # Create a candidate directory.
            candidate_dir = fs.mkdir("candidate", mock_fs.FSType.CARTFS)
            with open(
                os.path.join(candidate_dir, workspace.REPO_NAME_FILE_NAME), "w"
            ) as f:
                f.write("fuchsia")

            ws = workspace.Workspace(
                workspace_dir=os.path.join(
                    fs.cog_dir, "testuser/test-workspace"
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertEqual(result, candidate_dir)

    def test_find_previous_instance_ignores_current_workspace(self) -> None:
        """Test that the current workspace is ignored."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            ws = workspace.Workspace(
                workspace_dir=os.path.join(
                    fs.cog_dir, "testuser/test-workspace"
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertIsNone(result)

    def test_find_previous_instance_ignores_other_repo(self) -> None:
        """Test that the previous instance is skipped if it is for a different repository."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            # Create a candidate directory.
            candidate_dir = fs.mkdir("candidate", mock_fs.FSType.CARTFS)
            with open(
                os.path.join(candidate_dir, workspace.REPO_NAME_FILE_NAME), "w"
            ) as f:
                f.write("other-repo")

            ws = workspace.Workspace(
                workspace_dir=os.path.join(
                    fs.cog_dir, "testuser/test-workspace"
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertIsNone(result)

    def test_find_previous_instance_no_candidates(self) -> None:
        """Test that None is returned when there are no candidate directories."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            ws = workspace.Workspace(
                workspace_dir=os.path.join(
                    fs.cog_dir, "testuser/test-workspace"
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                cartfs_workspace_dir="",
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
