# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for workspace."""

import os
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import cartfs
import mock_fs
import workspace
from parameterized import parameterized


class TestCogMetadata(unittest.TestCase):
    """Tests for CogMetadata."""

    def test_to_dict(self) -> None:
        """Test that the dictionary representation is correct."""
        metadata = workspace.CogMetadata(
            workspace_name="test-ws", repo_name="fuchsia", workspace_id="ws-id"
        )
        self.assertEqual(
            metadata.to_dict(),
            {
                "workspace_name": "test-ws",
                "repo_name": "fuchsia",
                "workspace_id": "ws-id",
            },
        )

    def test_from_file_success(self) -> None:
        """Test that metadata can be loaded from a file."""
        with mock_fs.FileSystemTestHelper() as fs:
            metadata_path = (
                fs.cartfs_dir / "test-ws" / workspace.COG_METADATA_FILE_NAME
            )
            metadata_path.parent.mkdir(exist_ok=True, parents=True)
            with open(metadata_path, "w") as f:
                f.write(
                    '{"workspace_name": "test-ws", "repo_name": "fuchsia", "workspace_id": "ws-id"}'
                )

            metadata = workspace.CogMetadata.from_file(metadata_path)
            self.assertIsNotNone(metadata)

            # This assert is needed to make mypy happy
            assert metadata is not None
            self.assertEqual(metadata.workspace_name, "test-ws")
            self.assertEqual(metadata.repo_name, "fuchsia")
            self.assertEqual(metadata.workspace_id, "ws-id")

    def test_from_file_not_found(self) -> None:
        """Test that None is returned when the file does not exist."""
        with mock_fs.FileSystemTestHelper():
            metadata = workspace.CogMetadata.from_file(
                Path("non-existent-file")
            )
            self.assertIsNone(metadata)

    def test_from_file_invalid_json(self) -> None:
        """Test that None is returned when the file contains invalid JSON."""
        with mock_fs.FileSystemTestHelper() as fs:
            metadata_path = (
                fs.cartfs_dir / "test-ws" / workspace.COG_METADATA_FILE_NAME
            )
            metadata_path.parent.mkdir(exist_ok=True, parents=True)
            with open(metadata_path, "w") as f:
                f.write("invalid-json")
            metadata = workspace.CogMetadata.from_file(metadata_path)
            self.assertIsNone(metadata)

    def test_from_file_missing_keys(self) -> None:
        """Test that None is returned when the file is missing keys."""
        with mock_fs.FileSystemTestHelper() as fs:
            metadata_path = (
                fs.cartfs_dir / "test-ws" / workspace.COG_METADATA_FILE_NAME
            )
            metadata_path.parent.mkdir(exist_ok=True, parents=True)
            with open(metadata_path, "w") as f:
                f.write('{"workspace_name": "test-ws"}')
            metadata = workspace.CogMetadata.from_file(metadata_path)
            self.assertIsNone(metadata)

    def test_write(self) -> None:
        """Test that metadata can be written to a file."""
        with mock_fs.FileSystemTestHelper() as fs:
            metadata = workspace.CogMetadata(
                workspace_name="test-ws",
                repo_name="fuchsia",
                workspace_id="ws-id",
            )
            metadata.write(fs.cartfs_dir)
            self.assertEqual(
                fs.read(
                    workspace.COG_METADATA_FILE_NAME, mock_fs.FSType.CARTFS
                ),
                '{\n    "workspace_name": "test-ws",\n    "repo_name": "fuchsia",\n    "workspace_id": "ws-id"\n}',
            )


class TestCartfs(unittest.TestCase):
    """Tests for Cartfs."""

    def test_suggest_cartfs_directory_name_sanitizes_id(self) -> None:
        """Test that the workspace ID is sanitized."""
        with mock_fs.FileSystemTestHelper() as fs:
            c = cartfs.Cartfs(fs.cartfs_dir, use_local_mock_cartfs=False)
            suggested_name = c.suggest_cartfs_directory_name(
                workspace_name="test-ws", workspace_id="id/with/slashes"
            )
            self.assertEqual(str(suggested_name), "test-ws-id_with_slashes")


class TestWorkspace(unittest.TestCase):
    """Tests for Workspace."""

    def test_create_success(self) -> None:
        """Test that a Workspace instance can be created successfully."""
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            # Mock the environment variables and current working directory.
            with (
                patch.object(
                    os,
                    "getcwd",
                    return_value=fs.full_path(
                        "testuser/test-workspace/fuchsia", mock_fs.FSType.COG
                    ),
                ),
                patch.object(
                    cartfs.Cartfs, "create", return_value=MagicMock()
                ) as mock_cartfs_create,
            ):
                ws = workspace.Workspace.create()
                self.assertEqual(
                    ws.workspace_dir,
                    fs.cog_dir / "testuser" / "test-workspace",
                )
                self.assertEqual(ws.repo_name, "fuchsia")
                self.assertEqual(ws.workspace_name, "test-workspace")
                self.assertEqual(ws.workspace_id, fs.workspace_id)
                mock_cartfs_create.assert_called_once()

    def test_create_with_repo_root(self) -> None:
        """Test that a Workspace instance can be created successfully with a repo_root."""
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            # Mock the environment variables and current working directory.
            with (
                patch.object(
                    cartfs.Cartfs, "create", return_value=MagicMock()
                ) as mock_cartfs_create,
            ):
                ws = workspace.Workspace.create(
                    repo_root=fs.full_path(
                        "testuser/test-workspace/fuchsia", mock_fs.FSType.COG
                    )
                )
                self.assertEqual(
                    ws.workspace_dir,
                    fs.cog_dir / "testuser" / "test-workspace",
                )
                self.assertEqual(ws.repo_name, "fuchsia")
                self.assertEqual(ws.workspace_name, "test-workspace")
                self.assertEqual(ws.workspace_id, fs.workspace_id)
                mock_cartfs_create.assert_called_once()

    def test_create_not_in_cog_workspace(self) -> None:
        """Test that NotInCogWorkspaceError is raised when not in a Cog workspace."""
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            fs.mkdir("some/other/dir", mock_fs.FSType.COG)
            with (
                patch.object(
                    os,
                    "getcwd",
                    return_value=fs.full_path(
                        "some/other/dir", mock_fs.FSType.COG
                    ),
                ),
                self.assertRaises(workspace.NotInCogWorkspaceError),
            ):
                workspace.Workspace.create()

    def test_create_cannot_find_repo_name(self) -> None:
        """Test that CannotFindRepoNameError is raised when the repo name cannot be found."""
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            with (
                patch.object(
                    os,
                    "getcwd",
                    return_value=fs.full_path(
                        "testuser/test-workspace", mock_fs.FSType.COG
                    ),
                ),
                patch.object(cartfs.Cartfs, "create", return_value=MagicMock()),
                self.assertRaises(workspace.CannotFindRepoNameError),
            ):
                workspace.Workspace.create()

    def test_create_cartfs_error(self) -> None:
        """Test that CartfsError is raised when cartfs is not available."""
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            with (
                patch.object(
                    os,
                    "getcwd",
                    return_value=fs.full_path(
                        "testuser/test-workspace/fuchsia", mock_fs.FSType.COG
                    ),
                ),
                patch.object(
                    cartfs.Cartfs, "create", side_effect=cartfs.CartfsError
                ),
                self.assertRaises(cartfs.CartfsError),
            ):
                workspace.Workspace.create()

    @parameterized.expand(
        [
            (
                "testuser/test-workspace/fuchsia/src",
                "testuser/test-workspace",
            ),
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
        with mock_fs.FileSystemTestHelper(
            user="testuser",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            start_dir = fs.full_path(start_dir_suffix, mock_fs.FSType.COG)
            expected_dir = fs.full_path(expected_dir_suffix, mock_fs.FSType.COG)
            fs.mkdir(start_dir_suffix, mock_fs.FSType.COG)
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir
            )
            self.assertEqual(actual_dir, expected_dir)

    def test_find_cog_workspace_directory_not_found(self) -> None:
        """Test that None is returned when not in a Cog workspace."""
        with mock_fs.FileSystemTestHelper() as fs:
            start_dir = fs.full_path("some/other/dir", mock_fs.FSType.COG)
            fs.mkdir("some/other/dir", mock_fs.FSType.COG)
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir
            )
            self.assertIsNone(actual_dir)

    def test_find_cog_workspace_directory_at_root(self) -> None:
        """Test that None is returned when starting at the root directory."""
        with mock_fs.FileSystemTestHelper() as fs:
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                fs.cog_dir
            )
            self.assertIsNone(actual_dir)

    def test_find_cog_workspace_escapes_user(self) -> None:
        """Test that the workspace directory is found correctly."""
        with mock_fs.FileSystemTestHelper(
            user="test.user",
            workspace_name="test-workspace",
            repo_name="fuchsia",
        ) as fs:
            start_dir = fs.full_path(
                "test.user/test-workspace", mock_fs.FSType.COG
            )
            expected_dir = fs.full_path(
                "test.user/test-workspace", mock_fs.FSType.COG
            )
            actual_dir = workspace.Workspace._find_cog_workspace_directory(
                start_dir
            )
            self.assertEqual(actual_dir, expected_dir)

    @parameterized.expand(
        [
            (
                Path("/google/cog/cloud/testuser/test-workspace"),
                Path("/google/cog/cloud/testuser/test-workspace/fuchsia"),
                "fuchsia",
            ),
            (
                Path("/google/cog/cloud/testuser/test-workspace"),
                Path(
                    "/google/cog/cloud/testuser/test-workspace/fuchsia/out/default"
                ),
                "fuchsia",
            ),
        ]
    )
    def test_get_repo_name_from_path_success(
        self, workspace_dir: Path, path: Path, expected_repo_name: str
    ) -> None:
        """Test that the repo name is found correctly."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            workspace_dir, path
        )
        self.assertEqual(actual_repo_name, expected_repo_name)

    def test_get_repo_name_from_path_not_in_workspace(self) -> None:
        """Test that None is returned when the path is not in the workspace."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            Path("/google/cog/cloud/testuser/test-workspace"),
            Path("/some/other/dir"),
        )
        self.assertIsNone(actual_repo_name)

    def test_get_repo_name_from_path_same_as_workspace(self) -> None:
        """Test that None is returned when the path is the same as the workspace."""
        actual_repo_name = workspace.Workspace._get_repo_name_from_path(
            Path("/google/cog/cloud/testuser/test-workspace"),
            Path("/google/cog/cloud/testuser/test-workspace"),
        )
        self.assertIsNone(actual_repo_name)

    def test_get_linked_cartfs_workspace_directory_success(self) -> None:
        """Test that the cartfs workspace directory is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            # Setup cog workspace
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(Path(workspace_name) / repo_name, mock_fs.FSType.COG)

            # A symlink points from cog to cartfs
            fs.symlink_from_cog_to_cartfs(
                Path(workspace_name) / repo_name / workspace.CARTFS_SYMLINK_NAME
            )

            # A .cog.json file is created
            workspace.CogMetadata(
                repo_name=repo_name,
                workspace_name=workspace_name,
                workspace_id=fs.workspace_id,
            ).write(fs.cartfs_dir)

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG),
                    repo_name,
                    fs.workspace_id,
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
            fs.mkdir(Path(workspace_name) / repo_name, mock_fs.FSType.COG)

            workspace.CogMetadata(
                repo_name=repo_name,
                workspace_name=workspace_name,
                workspace_id=fs.workspace_id,
            ).write(fs.cartfs_dir)

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG),
                    repo_name,
                    fs.workspace_id,
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
            fs.mkdir(Path(workspace_name) / repo_name, mock_fs.FSType.COG)

            # A symlink points from cog to cartfs
            fs.symlink_from_cog_to_cartfs(
                Path(workspace_name) / repo_name / workspace.CARTFS_SYMLINK_NAME
            )
            fs.cartfs_dir.rmdir()

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG),
                    repo_name,
                    fs.workspace_id,
                )
            )
            self.assertIsNone(actual_dir)

    def test_get_linked_cartfs_workspace_directory_mismatch_id_fails(
        self,
    ) -> None:
        """Test that None is returned when the workspace ID does not match."""
        with mock_fs.FileSystemTestHelper() as fs:
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )
            fs.symlink_from_cog_to_cartfs(
                os.path.join(
                    workspace_name, repo_name, workspace.CARTFS_SYMLINK_NAME
                ),
            )
            workspace.CogMetadata(
                repo_name=repo_name,
                workspace_name=workspace_name,
                workspace_id="a-different-id",
            ).write(fs.cartfs_dir)

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG),
                    repo_name,
                    fs.workspace_id,
                )
            )
            self.assertIsNone(actual_dir)

    def test_get_linked_cartfs_workspace_directory_old_directory_fails(
        self,
    ) -> None:
        """Test that None is returned when the metadata does NOT have a workspace ID."""
        with mock_fs.FileSystemTestHelper() as fs:
            workspace_name = "test-workspace"
            repo_name = "fuchsia"
            fs.mkdir(
                os.path.join(workspace_name, repo_name), mock_fs.FSType.COG
            )
            fs.symlink_from_cog_to_cartfs(
                os.path.join(
                    workspace_name, repo_name, workspace.CARTFS_SYMLINK_NAME
                ),
            )
            workspace.CogMetadata(
                repo_name=repo_name,
                workspace_name=workspace_name,
                workspace_id=None,
            ).write(fs.cartfs_dir)

            actual_dir = (
                workspace.Workspace.get_linked_cartfs_workspace_directory(
                    fs.full_path(workspace_name, mock_fs.FSType.COG),
                    repo_name,
                    fs.workspace_id,
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
                _workspace_to_snapshot_from: Path,
                _workspace_to_snapshot_to: Path,
                cartfs_mount_point: Path,
            ) -> None:
                (cartfs_mount_point / suggested_directory_name).mkdir()

            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )
            with patch.object(
                ws,
                "_find_previous_instance",
                return_value=Path("foo"),
            ):
                result = ws.snapshot_from_previous_instance(
                    snapshot_function=mock_snapshot_workspace,
                )
                self.assertEqual(
                    result,
                    Path(cartfs_instance.mount_point)
                    / suggested_directory_name,
                )
                self.assertTrue(
                    fs.full_path(
                        suggested_directory_name, mock_fs.FSType.CARTFS
                    ).is_dir()
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
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
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
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )
            with patch.object(
                ws,
                "_find_previous_instance",
                return_value=fs.cartfs_dir / "previous_instance",
            ):

                def mock_snapshot_workspace(
                    _workspace_to_snapshot_from: Path,
                    _workspace_to_snapshot_to: Path,
                    _cartfs_mount_point: Path,
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
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )

            result = ws.create_empty_cartfs_workspace_directory()

            expected_dir = (
                Path(cartfs_instance.mount_point) / suggested_directory_name
            )
            self.assertEqual(result, expected_dir)
            self.assertTrue(expected_dir.is_dir())

    def test_link_to_cartfs(self) -> None:
        """Test that the workspace can be linked to a cartfs directory."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            workspace_name = "test-workspace"
            workspace_dir = fs.full_path(workspace_name, mock_fs.FSType.COG)
            repo_name = "fuchsia"
            repo_dir = fs.mkdir(
                Path(workspace_name) / repo_name,
                mock_fs.FSType.COG,
            )

            ws = workspace.Workspace(
                workspace_dir=workspace_dir,
                repo_name=repo_name,
                workspace_name=workspace_name,
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )

            cartfs_directory = fs.mkdir(
                "cartfs_directory", mock_fs.FSType.CARTFS
            )
            ws.link_to_cartfs(cartfs_directory)

            symlink_path = repo_dir / workspace.CARTFS_SYMLINK_NAME
            self.assertTrue(symlink_path.is_symlink())

            # Ensure that we write the name of the repository in cartfs
            metadata = workspace.CogMetadata.from_file(
                cartfs_directory / workspace.COG_METADATA_FILE_NAME
            )
            self.assertIsNotNone(metadata)

            self.assertEqual(
                metadata and metadata.repo_name or "",
                "fuchsia",
            )
            self.assertEqual(
                metadata and metadata.workspace_name or "",
                "test-workspace",
            )
            self.assertEqual(
                metadata and metadata.workspace_id or "",
                fs.workspace_id,
            )

    def test_find_previous_instance_success(self) -> None:
        """Test that the previous instance is found correctly."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            # Create a candidate directory.
            candidate_dir = fs.mkdir("candidate", mock_fs.FSType.CARTFS)
            workspace.CogMetadata(
                workspace_name="test-workspace",
                repo_name="fuchsia",
            ).write(candidate_dir)

            ws = workspace.Workspace(
                workspace_dir=fs.cog_dir / "testuser" / "test-workspace",
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertEqual(result, Path("candidate"))

    def test_find_previous_instance_ignores_current_workspace(self) -> None:
        """Test that the current workspace is ignored."""
        with mock_fs.FileSystemTestHelper() as fs:
            cartfs_instance = MagicMock()
            cartfs_instance.mount_point = fs.cartfs_dir

            ws = workspace.Workspace(
                workspace_dir=fs.cog_dir / "testuser" / "test-workspace",
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
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
            workspace.CogMetadata(
                workspace_name="test-workspace",
                repo_name="other-repo",
            ).write(candidate_dir)

            ws = workspace.Workspace(
                workspace_dir=fs.cog_dir / "testuser" / "test-workspace",
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
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
                workspace_dir=fs.cog_dir / "testuser" / "test-workspace",
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=None,
                cartfs_instance=cartfs_instance,
            )

            result = ws._find_previous_instance()
            self.assertIsNone(result)

    def test_initialize_cartfs_directory_up_to_date(self) -> None:
        """Test that initialization skips sync when up-to-date but still creates symlinks."""
        with mock_fs.FileSystemTestHelper() as fs:
            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=fs.cartfs_dir,
                cartfs_instance=MagicMock(),
            )

            with (
                patch.object(
                    ws, "get_fuchsia_repo_commit_hash", return_value="hash123"
                ),
                patch.object(ws, "is_up_to_date", return_value=True),
                patch.object(ws, "_sync_fuchsia_repo") as mock_sync,
                patch.object(ws, "_create_symlinks") as mock_symlinks,
            ):
                ws.initialize_cartfs_directory()

                mock_sync.assert_not_called()
                mock_symlinks.assert_called_once()

    def test_initialize_cartfs_directory_not_up_to_date(self) -> None:
        """Test that initialization performs sync when not up-to-date."""
        with mock_fs.FileSystemTestHelper() as fs:
            ws = workspace.Workspace(
                workspace_dir=fs.full_path(
                    "test-workspace", mock_fs.FSType.COG
                ),
                repo_name="fuchsia",
                workspace_name="test-workspace",
                workspace_id=fs.workspace_id,
                cartfs_directory=fs.cartfs_dir,
                cartfs_instance=MagicMock(),
            )

            with (
                patch.object(
                    ws, "get_fuchsia_repo_commit_hash", return_value="hash123"
                ),
                patch.object(ws, "is_up_to_date", return_value=False),
                patch.object(ws, "_sync_fuchsia_repo") as mock_sync,
                patch.object(ws, "_create_symlinks") as mock_symlinks,
            ):
                ws.initialize_cartfs_directory()

                mock_sync.assert_called_once_with("hash123")
                mock_symlinks.assert_called_once()


if __name__ == "__main__":
    unittest.main()
