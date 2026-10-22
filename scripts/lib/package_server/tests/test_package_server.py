# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import os
import tempfile
import typing
import unittest
import unittest.mock as mock
from pathlib import Path

import package_server
from async_utils.command import AsyncCommandError, CommandOutput


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

    @mock.patch("package_server.lib.FxCmd")
    async def test_deregister(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        output = CommandOutput("", "", 0, 0.1, None)
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=output)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertEqual(
            await package_server.deregister(name="my-repo"),
            output,
        )
        mock_fx_cmd.assert_called_once_with(build_directory=None, timeout=5.0)
        mock_fx_cmd.return_value.start.assert_called_once_with(
            "ffx",
            "target",
            "repository",
            "deregister",
            "-r",
            "my-repo",
        )

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

    def test_is_fuchsia_repo_registered(self) -> None:
        data_aliases = json.dumps(
            {"ok": {"data": [{"name": "devhost", "aliases": ["fuchsia.com"]}]}}
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_aliases))

        data_name = json.dumps(
            {"ok": {"data": [{"name": "fuchsia.com", "aliases": []}]}}
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_name))

        data_pkg = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "fuchsia-pkg://fuchsia.com", "aliases": []}
                    ]
                }
            }
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_pkg))

        self.assertFalse(
            package_server.is_fuchsia_repo_registered(
                json.dumps({"ok": {"data": []}})
            )
        )

        with self.assertRaises(json.JSONDecodeError):
            package_server.is_fuchsia_repo_registered("not json")

        data_multiple = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "stale-repo", "aliases": ["fuchsia.com"]},
                        {"name": "fxtest-abc123", "aliases": ["fuchsia.com"]},
                    ]
                }
            }
        )
        self.assertTrue(
            package_server.is_fuchsia_repo_registered(
                data_multiple, repo_name="fxtest-abc123"
            )
        )
        self.assertFalse(
            package_server.is_fuchsia_repo_registered(
                data_multiple, repo_name="fxtest-other"
            )
        )

        data_pkg_repo = json.dumps(
            {
                "ok": {
                    "data": [
                        {
                            "name": "fuchsia-pkg://fxtest-abc123",
                            "aliases": ["fuchsia.com"],
                        }
                    ]
                }
            }
        )
        self.assertTrue(
            package_server.is_fuchsia_repo_registered(
                data_pkg_repo, repo_name="fxtest-abc123"
            )
        )

    async def test_is_connected_to_device(self) -> None:
        runner = mock.AsyncMock(
            side_effect=[
                CommandOutput("ok", "", 0, 0.1, None),
                CommandOutput(
                    json.dumps(
                        {
                            "ok": {
                                "data": [
                                    {
                                        "name": "devhost",
                                        "aliases": ["fuchsia.com"],
                                    }
                                ]
                            }
                        }
                    ),
                    "",
                    0,
                    0.1,
                    None,
                ),
            ]
        )
        self.assertTrue(
            await package_server.is_connected_to_device(run_command=runner)
        )
        self.assertEqual(runner.call_count, 2)

        runner_not_running = mock.AsyncMock(
            return_value=CommandOutput("", "", 1, 0.1, None)
        )
        self.assertFalse(
            await package_server.is_connected_to_device(
                run_command=runner_not_running
            )
        )
        self.assertEqual(runner_not_running.call_count, 1)

        runner_bad_json = mock.AsyncMock(
            side_effect=[
                CommandOutput("ok", "", 0, 0.1, None),
                CommandOutput("not json", "", 0, 0.1, None),
            ]
        )
        self.assertFalse(
            await package_server.is_connected_to_device(
                run_command=runner_bad_json
            )
        )
        self.assertEqual(runner_bad_json.call_count, 2)

    async def test_wait_for_registration(self) -> None:
        stale_output = json.dumps(
            {
                "ok": {
                    "data": [{"name": "stale-repo", "aliases": ["fuchsia.com"]}]
                }
            }
        )
        registered_output = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "stale-repo", "aliases": ["fuchsia.com"]},
                        {"name": "fxtest-1234", "aliases": ["fuchsia.com"]},
                    ]
                }
            }
        )
        runner = mock.AsyncMock(
            side_effect=[
                CommandOutput(stale_output, "", 0, 0.1, None),
                CommandOutput(registered_output, "", 0, 0.1, None),
            ]
        )
        with mock.patch("asyncio.sleep", new_callable=mock.AsyncMock):
            self.assertTrue(
                await package_server.wait_for_registration(
                    repo_name="fxtest-1234", run_command=runner
                )
            )
        self.assertEqual(runner.call_count, 2)

        runner_exhausted = mock.AsyncMock(
            return_value=CommandOutput(stale_output, "", 0, 0.1, None)
        )
        with mock.patch("asyncio.sleep", new_callable=mock.AsyncMock):
            self.assertFalse(
                await package_server.wait_for_registration(
                    repo_name="fxtest-1234",
                    retries=3,
                    run_command=runner_exhausted,
                )
            )
        self.assertEqual(runner_exhausted.call_count, 3)

    async def test_serve_lifecycle_and_warnings(self) -> None:
        events: list[str] = []
        warnings: list[str] = []
        serve_abort_signal: asyncio.Event | None = None
        deregister_calls = 0

        async def fake_runner(
            *args: str, **kwargs: typing.Any
        ) -> CommandOutput | None:
            nonlocal serve_abort_signal, deregister_calls
            if "deregister" in args:
                deregister_calls += 1
                events.append(f"deregister_{deregister_calls}")
                # Fail on teardown deregistration to exercise warning callback.
                rc = 0 if deregister_calls == 1 else 1
                return CommandOutput("", "", rc, 0.1, None)
            if "serve" in args:
                events.append("serve")
                serve_abort_signal = kwargs.get("abort_signal")
                if serve_abort_signal is not None:
                    await serve_abort_signal.wait()
                return CommandOutput("", "", 0, 0.1, None)
            return CommandOutput("", "", 0, 0.1, None)

        task, cancel_event = await package_server.serve(
            name="fxtest-1234",
            run_command=fake_runner,
            on_warning=warnings.append,
        )
        cancel_event.set()
        await task

        self.assertEqual(events, ["deregister_1", "serve", "deregister_2"])
        self.assertIsNotNone(serve_abort_signal)
        assert serve_abort_signal is not None
        self.assertTrue(serve_abort_signal.is_set())
        self.assertEqual(len(warnings), 1)
        self.assertIn(
            "Failed to deregister temporary package repository fxtest-1234",
            warnings[0],
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_serve_forwards_build_directory_to_default_runner(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        cmd_mock = mock.Mock()
        cmd_mock.terminate = mock.Mock()
        cmd_mock.kill = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(
            return_value=CommandOutput("", "", 0, 0.1, None)
        )
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        task, cancel_event = await package_server.serve(
            prefix="fxtest",
            build_directory=Path("/custom/out/dir"),
        )
        cancel_event.set()
        await task

        for call in mock_fx_cmd.call_args_list:
            self.assertEqual(
                call.kwargs.get("build_directory"), "/custom/out/dir"
            )

    async def test_serve_stop_timeout_emits_warning(self) -> None:
        warnings: list[str] = []
        serve_cancelled = False

        async def hanging_runner(
            *args: str, **kwargs: typing.Any
        ) -> CommandOutput | None:
            nonlocal serve_cancelled
            if "serve" in args:
                try:
                    await asyncio.Future()
                except asyncio.CancelledError:
                    serve_cancelled = True
                    raise
            return CommandOutput("", "", 0, 0.1, None)

        with mock.patch(
            "package_server.lib._DEFAULT_SERVER_STOP_TIMEOUT", 0.01
        ):
            task, cancel_event = await package_server.serve(
                name="fxtest-1234",
                run_command=hanging_runner,
                on_warning=warnings.append,
            )
            cancel_event.set()
            await task

        self.assertTrue(serve_cancelled)
        self.assertEqual(len(warnings), 1)
        self.assertIn(
            "Timed out waiting for temporary package server (fxtest-1234) to stop.",
            warnings[0],
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_abort_signal_terminates_and_kills(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        runner_done = asyncio.Event()
        cmd_mock = mock.Mock()

        async def wait_until_killed() -> CommandOutput:
            await runner_done.wait()
            return CommandOutput("", "", -9, 0.1, None)

        cmd_mock.terminate = mock.Mock()
        cmd_mock.kill = mock.Mock(side_effect=runner_done.set)
        cmd_mock.run_to_completion = mock.AsyncMock(
            side_effect=wait_until_killed
        )
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        abort_signal = asyncio.Event()
        runner = package_server.lib._default_command_runner()
        with mock.patch("package_server.lib._DEFAULT_ABORT_GRACE_PERIOD", 0.01):
            run_task = asyncio.create_task(
                runner("serve", "-l", "0", abort_signal=abort_signal)
            )
            await asyncio.sleep(0)
            abort_signal.set()
            await run_task

        mock_fx_cmd.return_value.start.assert_called_once_with(
            "serve", "-l", "0"
        )
        cmd_mock.terminate.assert_called_once()
        cmd_mock.kill.assert_called_once()

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_cancellation_kills_process(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        async def never_finishes() -> CommandOutput:
            await asyncio.Future()
            return CommandOutput("", "", 0, 0.1, None)

        cmd_mock = mock.Mock()
        cmd_mock.kill = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(side_effect=never_finishes)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        runner = package_server.lib._default_command_runner()
        run_task = asyncio.create_task(runner("serve", "-l", "0"))
        await asyncio.sleep(0)
        run_task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await run_task

        cmd_mock.kill.assert_called_once()

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_handles_async_command_error(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        mock_fx_cmd.return_value.start = mock.AsyncMock(
            side_effect=AsyncCommandError("failed")
        )
        runner = package_server.lib._default_command_runner()
        self.assertIsNone(await runner("is-package-server-running"))
