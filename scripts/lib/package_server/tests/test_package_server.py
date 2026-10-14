# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import os
import tempfile
import unittest
import unittest.mock as mock
from pathlib import Path

import package_server


class TestPackageServer(unittest.IsolatedAsyncioTestCase):
    @mock.patch("package_server.lib.FxCmd")
    async def test_is_running_true(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.return_code = 0
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        # Configure start to be an async mock that returns cmd_mock
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertTrue(await package_server.is_running())
        mock_fx_cmd.return_value.start.assert_called_with(
            "is-package-server-running"
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_is_running_false(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.return_code = 1
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertFalse(await package_server.is_running())

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_success(
        self, mock_is_running: mock.Mock
    ) -> None:
        mock_is_running.return_value = True
        process_mock = mock.Mock()
        process_mock.wait = mock.Mock(return_value=asyncio.Future())

        self.assertTrue(
            await package_server.wait_for_package_server(
                process_mock, timeout=1, interval=0.1
            )
        )

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_process_exits(
        self, mock_is_running: mock.Mock
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.wait = mock.AsyncMock(return_value=None)

        self.assertFalse(
            await package_server.wait_for_package_server(
                process_mock, timeout=1, interval=0.1
            )
        )

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_timeout(
        self,
        mock_is_running: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.wait = mock.Mock(return_value=asyncio.Future())

        self.assertFalse(
            await package_server.wait_for_package_server(
                process_mock, timeout=0.01, interval=0.001
            )
        )

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    @mock.patch("package_server.lib.FfxCmd")
    async def test_get_arguments(
        self, mock_ffx_cmd: mock.Mock, mock_get_build_dir: mock.Mock
    ) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.stdout = "8084\n"
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        mock_get_build_dir.return_value = Path("/some/out/dir")

        args = await package_server.get_arguments()
        self.assertIn("server", args)
        self.assertIn("start", args)
        self.assertIn("[::]:8084", args)
        self.assertIn(package_server.ephemeral_repository_name(), args)
        self.assertNotIn("fx", args)
        self.assertNotIn("ffx", args)
        mock_ffx_cmd.return_value.start.assert_called_with(
            "config",
            "get",
            "repository.server.default_port",
        )

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    def test_ephemeral_repository_name(
        self, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_get_build_dir.return_value = Path("/some/out/dir")
        name1 = package_server.ephemeral_repository_name()
        name2 = package_server.ephemeral_repository_name()
        fxtest_name = package_server.ephemeral_repository_name("fxtest")
        explicit_dir_name = package_server.ephemeral_repository_name(
            "fxtest", "/some/out/dir"
        )
        mock_get_build_dir.return_value = Path("/other/out/dir")
        name3 = package_server.ephemeral_repository_name()

        self.assertEqual(name1, name2)
        self.assertNotEqual(name1, name3)
        self.assertEqual(fxtest_name, explicit_dir_name)
        self.assertRegex(name1, r"^tmp-[0-9a-f]{16}$")
        self.assertRegex(fxtest_name, r"^fxtest-[0-9a-f]{16}$")
        self.assertEqual(name1.split("-", 1)[1], fxtest_name.split("-", 1)[1])

        with tempfile.TemporaryDirectory() as td:
            real_out = os.path.join(td, "real_out")
            os.makedirs(os.path.join(real_out, "amber-files"))
            link_out = os.path.join(td, "link_out")
            os.symlink(real_out, link_out)
            self.assertEqual(
                package_server.ephemeral_repository_name("fxtest", real_out),
                package_server.ephemeral_repository_name("fxtest", link_out),
            )

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    @mock.patch("package_server.lib.FfxCmd")
    async def test_get_arguments_with_name(
        self, mock_ffx_cmd: mock.Mock, mock_get_build_dir: mock.Mock
    ) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.stdout = "8083\n"
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        mock_get_build_dir.return_value = Path("/some/out/dir")

        args = await package_server.get_arguments(name="my-repo")
        self.assertIn("my-repo", args)

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    @mock.patch("package_server.lib.FfxCmd")
    async def test_get_arguments_ffx_config_failure(
        self, mock_ffx_cmd: mock.Mock, mock_get_build_dir: mock.Mock
    ) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.stdout = "invalid\n"
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        mock_get_build_dir.return_value = Path("/some/out/dir")

        args = await package_server.get_arguments()
        # Should fallback to default port 8083
        self.assertIn("[::]:8083", args)

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    def test_is_package_repository_built(
        self, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_path = mock.MagicMock()
        mock_get_build_dir.return_value = mock_path
        repo_json = mock_path / "amber-files" / "repository" / "9.root.json"

        repo_json.is_file.return_value = True
        self.assertTrue(package_server.is_package_repository_built())

        repo_json.is_file.return_value = False
        self.assertFalse(package_server.is_package_repository_built())

    @mock.patch(
        "package_server.lib.build_dir.get_build_directory",
        return_value=Path("/some/out/dir"),
    )
    @mock.patch("package_server.lib.is_package_repository_built")
    @mock.patch("package_server.lib.wait_for_package_server")
    @mock.patch("package_server.lib.get_arguments")
    @mock.patch("package_server.lib.FfxCmd")
    @mock.patch("package_server.lib.FxCmd")
    @mock.patch("asyncio.create_subprocess_exec")
    async def test_start_success(
        self,
        mock_exec: mock.Mock,
        mock_fx_cmd: mock.Mock,
        mock_ffx_cmd: mock.Mock,
        mock_get_arguments: mock.Mock,
        mock_wait: mock.Mock,
        mock_is_built: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_built.return_value = True
        mock_wait.return_value = True
        mock_get_arguments.return_value = ("arg1", "arg2")
        mock_ffx_cmd.return_value.command_line.side_effect = (
            lambda *args: ("ffx",) + args
        )
        mock_fx_cmd.return_value.command_line.side_effect = (
            lambda *args: ("fx",) + args
        )

        # Mock for package server process
        server_process_mock = mock.Mock()
        mock_exec.return_value = server_process_mock

        self.assertEqual(await package_server.start(), server_process_mock)
        mock_get_arguments.assert_called_once_with(
            package_server.ephemeral_repository_name()
        )
        mock_ffx_cmd.return_value.command_line.assert_called_with(
            "arg1", "arg2"
        )
        mock_fx_cmd.return_value.command_line.assert_called_with(
            "ffx", "arg1", "arg2"
        )
        mock_exec.assert_called_with(
            "fx",
            "ffx",
            "arg1",
            "arg2",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        mock_wait.assert_called_with(server_process_mock, 30, 0.2)

    @mock.patch("package_server.lib.is_package_repository_built")
    async def test_start_fails_when_repo_not_built(
        self,
        mock_is_built: mock.Mock,
    ) -> None:
        mock_is_built.return_value = False
        with self.assertRaisesRegex(
            package_server.PackageServingException,
            "The package repository is not built!",
        ):
            await package_server.start()

    @mock.patch("package_server.lib.FfxCmd")
    async def test_stop(self, mock_ffx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        await package_server.stop(name="my-repo")
        mock_ffx_cmd.return_value.start.assert_called_with(
            "repository",
            "server",
            "stop",
            "my-repo",
        )
        cmd_mock.run_to_completion.assert_called()

    @mock.patch("package_server.lib.FfxCmd")
    async def test_stop_all(self, mock_ffx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        await package_server.stop()
        mock_ffx_cmd.return_value.start.assert_called_with(
            "repository",
            "server",
            "stop",
        )
        cmd_mock.run_to_completion.assert_called()

    @mock.patch("package_server.lib.FfxCmd")
    async def test_deregister(self, mock_ffx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock()
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        await package_server.deregister("my-repo")
        mock_ffx_cmd.return_value.start.assert_called_once_with(
            "target",
            "repository",
            "deregister",
            "-r",
            "my-repo",
        )
        cmd_mock.run_to_completion.assert_called_once()

    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_already_running(
        self,
        mock_is_running: mock.Mock,
    ) -> None:
        mock_is_running.return_value = True
        async with package_server.run_and_stop_on_exit():
            pass
        mock_is_running.assert_called()

    @mock.patch(
        "package_server.lib.build_dir.get_build_directory",
        return_value=Path("/some/out/dir"),
    )
    @mock.patch("package_server.lib.deregister")
    @mock.patch("package_server.lib.stop")
    @mock.patch("package_server.lib.start")
    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_starts_and_stops(
        self,
        mock_is_running: mock.Mock,
        mock_start: mock.Mock,
        mock_stop: mock.Mock,
        mock_deregister: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.terminate = mock.Mock()
        process_mock.wait = mock.AsyncMock(return_value=None)
        mock_start.return_value = process_mock

        async with package_server.run_and_stop_on_exit():
            pass

        expected_repo_name = package_server.ephemeral_repository_name()
        mock_is_running.assert_called()
        mock_start.assert_called_once_with(expected_repo_name)
        mock_deregister.assert_called_once_with(expected_repo_name)
        mock_stop.assert_called_once_with(expected_repo_name)

        process_mock.terminate.assert_called()
        process_mock.wait.assert_called()

    @mock.patch(
        "package_server.lib.build_dir.get_build_directory",
        return_value=Path("/some/out/dir"),
    )
    @mock.patch("package_server.lib.start")
    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_failure(
        self,
        mock_is_running: mock.Mock,
        mock_start: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        mock_start.side_effect = package_server.PackageServingException("fail")

        with self.assertRaises(package_server.PackageServingCLIException):
            async with package_server.run_and_stop_on_exit():
                pass
