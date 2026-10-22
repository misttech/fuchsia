# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import hashlib
import json
import os
import shlex
from contextlib import asynccontextmanager
from typing import AsyncGenerator, Callable, Literal, Protocol

import build_dir
from async_utils.command import AsyncCommand, AsyncCommandError, CommandOutput
from ffx_cmd import FfxCmd
from fx_cmd import FxCmd

_DEFAULT_SERVER_STARTUP_TIMEOUT = 30
_CHECK_SERVER_POLLING_INTERVAL = 0.2
_DEFAULT_PROBE_TIMEOUT = 15.0
_DEFAULT_DEREGISTER_TIMEOUT = 5.0
_DEFAULT_SERVER_STOP_TIMEOUT = 10.0
_DEFAULT_ABORT_GRACE_PERIOD = 5.0


class CommandRunner(Protocol):
    """Callable that executes an `fx` subcommand asynchronously.

    Args:
        *args (str): Subcommand and arguments to pass to `fx`.
        timeout (float | None): Optional timeout in seconds before terminating
            the command.
        abort_signal (asyncio.Event | None): Optional event that, when set,
            requests graceful termination of the running command.
        quiet_mode (bool): Whether to suppress command lifecycle output in
            interactive status displays.

    Returns:
        CommandOutput | None: Completed command output, or None if the command
            could not be started.
    """

    async def __call__(
        self,
        *args: str,
        timeout: float | None = None,
        abort_signal: asyncio.Event | None = None,
        quiet_mode: bool = False,
    ) -> CommandOutput | None:
        ...


def _default_command_runner(
    build_directory: str | os.PathLike[str] | None = None,
) -> CommandRunner:
    """Creates a `CommandRunner` backed by `FxCmd` and `AsyncCommand`."""
    resolved_build_dir = (
        os.fspath(build_directory) if build_directory is not None else None
    )

    async def run_fx(
        *args: str,
        timeout: float | None = None,
        abort_signal: asyncio.Event | None = None,
        quiet_mode: bool = False,
    ) -> CommandOutput | None:
        del quiet_mode
        started: AsyncCommand | None = None
        abort_task: asyncio.Task[None] | None = None
        try:
            fx_cmd = FxCmd(
                build_directory=resolved_build_dir,
                timeout=timeout,
            )
            started = await fx_cmd.start(*args)

            if abort_signal is not None:

                async def handle_abort() -> None:
                    await abort_signal.wait()
                    started.terminate()
                    await asyncio.sleep(_DEFAULT_ABORT_GRACE_PERIOD)
                    started.kill()

                abort_task = asyncio.create_task(handle_abort())

            return await started.run_to_completion()
        except asyncio.CancelledError:
            if started is not None:
                started.kill()
            raise
        except AsyncCommandError:
            return None
        finally:
            if abort_task is not None:
                abort_task.cancel()

    return run_fx


class PackageServingException(Exception):
    """This exception is raised on unsuccessful package serving attempts."""


class PackageServingCLIException(SystemExit, PackageServingException):
    """
    Like `PackageServingException`, but does not print a stack trace when uncaught.

    Used in the `run_and_stop_on_exit()` asynccontextmanager function, where concise error messages
    are more helpful than noisy stack traces for CLI developer tools.
    """


async def is_running(
    *,
    timeout: float | None = None,
    run_command: CommandRunner | None = None,
) -> bool:
    """
    Checks whether a package server is currently running.

    Args:
        timeout (float | None): Subprocess timeout in seconds.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.

    Returns:
        bool: Whether a package server is currently running.
    """
    runner = run_command or _default_command_runner()
    result = await runner("is-package-server-running", timeout=timeout)
    return result is not None and result.return_code == 0


def is_fuchsia_repo_registered(
    output: str, repo_name: str | None = None
) -> bool:
    """
    Checks whether a `fuchsia.com` repository is present in `ffx` JSON output.

    Args:
        output (str): JSON stdout from `ffx --machine json target repository list`.
        repo_name (str | None): Optional repository name to require when
            matching `fuchsia.com` registrations.

    Returns:
        bool: True if a matching `fuchsia.com` repository entry is present.

    Raises:
        json.JSONDecodeError: If `output` is not valid JSON.
    """
    parsed = json.loads(output)
    # Expected format from ffx --machine json: {"ok": {"data": [...]}}
    entries = []
    if isinstance(parsed, dict) and "ok" in parsed:
        data = parsed["ok"].get("data", [])
        if isinstance(data, list):
            entries = data
    elif isinstance(parsed, list):
        entries = parsed
    for entry in entries:
        if isinstance(entry, dict):
            name = entry.get("name", "").removeprefix("fuchsia-pkg://")
            if repo_name is not None and name != repo_name:
                continue
            aliases = entry.get("aliases", [])
            if name == "fuchsia.com" or "fuchsia.com" in aliases:
                return True
    return False


async def is_fuchsia_repo_registered_on_device(
    *,
    repo_name: str | None = None,
    timeout: float = _DEFAULT_PROBE_TIMEOUT,
    run_command: CommandRunner | None = None,
) -> bool:
    """
    Checks whether a `fuchsia.com` repository is registered on the target device.

    Args:
        repo_name (str | None): Optional repository name to require when
            matching `fuchsia.com` registrations.
        timeout (float): Subprocess timeout in seconds.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.

    Returns:
        bool: True if a matching `fuchsia.com` repository is registered on the
            target device.
    """
    runner = run_command or _default_command_runner()
    repo_output = await runner(
        "ffx",
        "--machine",
        "json",
        "target",
        "repository",
        "list",
        timeout=timeout,
        quiet_mode=True,
    )
    if repo_output is None or repo_output.return_code != 0:
        return False
    try:
        return is_fuchsia_repo_registered(
            repo_output.stdout, repo_name=repo_name
        )
    except json.JSONDecodeError:
        return False


async def is_connected_to_device(
    *,
    repo_name: str | None = None,
    timeout: float = _DEFAULT_PROBE_TIMEOUT,
    run_command: CommandRunner | None = None,
) -> bool:
    """
    Checks whether a package server is running and registered on the target device.

    Args:
        repo_name (str | None): Optional repository name to require when
            matching `fuchsia.com` registrations on the target.
        timeout (float): Subprocess timeout in seconds.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.

    Returns:
        bool: True only if a host package server is running and registered on
            the target device.
    """
    if not await is_running(
        timeout=timeout,
        run_command=run_command,
    ):
        return False
    return await is_fuchsia_repo_registered_on_device(
        repo_name=repo_name,
        timeout=timeout,
        run_command=run_command,
    )


async def wait_for_registration(
    *,
    repo_name: str | None = None,
    retries: int = 30,
    interval: float = 1.0,
    timeout: float = _DEFAULT_PROBE_TIMEOUT,
    run_command: CommandRunner | None = None,
) -> bool:
    """
    Polls until a `fuchsia.com` repository is registered on the target device.

    Args:
        repo_name (str | None): Optional repository name to require when
            matching `fuchsia.com` registrations. Filtering by an expected
            ephemeral repository name prevents unrelated stale registrations
            from causing the wait to return early.
        retries (int): Maximum number of polling attempts.
        interval (float): Delay in seconds between polling attempts.
        timeout (float): Per-probe subprocess timeout in seconds.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.

    Returns:
        bool: True if the repository is registered within `retries` attempts.
    """
    runner = run_command or _default_command_runner()
    for _ in range(retries):
        if await is_fuchsia_repo_registered_on_device(
            repo_name=repo_name,
            timeout=timeout,
            run_command=runner,
        ):
            return True
        await asyncio.sleep(interval)
    return False


async def deregister(
    name: str,
    *,
    timeout: float = _DEFAULT_DEREGISTER_TIMEOUT,
    run_command: CommandRunner | None = None,
) -> CommandOutput | None:
    """
    Deregisters a package repository from the target device.

    Args:
        name (str): Repository name to deregister from the target.
        timeout (float): Subprocess timeout in seconds.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.

    Returns:
        CommandOutput | None: Subprocess output, or None if execution failed.
    """
    runner = run_command or _default_command_runner()
    return await runner(
        "ffx",
        "target",
        "repository",
        "deregister",
        "-r",
        name,
        timeout=timeout,
        quiet_mode=True,
    )


async def serve(
    *,
    name: str | None = None,
    prefix: str = "tmp",
    build_directory: str | os.PathLike[str] | None = None,
    run_command: CommandRunner | None = None,
    on_warning: Callable[[str], None] | None = None,
) -> tuple[asyncio.Task[None], asyncio.Event]:
    """
    Starts a managed background package server and returns its control handles.

    Deregisters any stale target registration for the repository name before
    starting the server, and when the returned event is set, deregisters the
    repository from the target before terminating the server process group.

    Args:
        name (str | None): Explicit repository name, or None to derive a
            deterministic ephemeral name using `prefix` and `build_directory`.
        prefix (str): Tool-specific prefix when deriving an ephemeral repository
            name.
        build_directory (str | PathLike[str] | None): Build directory whose
            package repository is being served, or None for the active build
            directory.
        run_command (CommandRunner | None): Optional runner for `fx`
            subcommands.
        on_warning (Callable[[str], None] | None): Optional callback invoked
            with a human-readable warning message if teardown deregistration or
            server shutdown fails.

    Returns:
        tuple[Task[None], Event]: Background lifecycle task and the event that
            triggers graceful deregistration and server shutdown when set.

    Raises:
        build_dir.GetBuildDirectoryError: If `build_directory` is None (when
            deriving `name` or using the default command runner) and no active
            build directory can be determined.
    """
    cancel_event = asyncio.Event()
    repo_name = name or ephemeral_repository_name(prefix, build_directory)
    runner = run_command or _default_command_runner(
        build_directory=build_directory
    )

    # Deregister any stale registration left on the device by an interrupted
    # prior run so `wait_for_registration` waits for the newly started server
    # instance to register its port.
    await deregister(repo_name, run_command=runner)

    started_event = asyncio.Event()
    repo_deregistered_event = asyncio.Event()

    # Aborting the `fx serve` task stops the repository server but does not
    # remove the corresponding repository config or rewrite rule from the
    # device. The repository config is explicitly deregistered from the device
    # (which also removes the rewrite rule) before cancelling the server task to
    # avoid the pkg-resolver's AutoClient logging that the server has gone away.
    async def impl() -> None:
        serve_task: asyncio.Task[CommandOutput | None] | None = None
        try:
            serve_task = asyncio.create_task(
                runner(
                    "serve",
                    "-l",
                    "0",
                    "--name",
                    repo_name,
                    abort_signal=repo_deregistered_event,
                    quiet_mode=True,
                )
            )
            started_event.set()
            await cancel_event.wait()
        finally:
            try:
                deregister_output = await deregister(
                    repo_name,
                    run_command=runner,
                )
                if (
                    deregister_output is None
                    or deregister_output.return_code != 0
                ):
                    raise RuntimeError(
                        f"exit code {deregister_output.return_code if deregister_output is not None else -1}"
                    )
            except Exception as e:
                if on_warning is not None:
                    on_warning(
                        f"Failed to deregister temporary package repository {repo_name}: {e}"
                    )
            finally:
                repo_deregistered_event.set()
                if serve_task is not None:
                    try:
                        await asyncio.wait_for(
                            serve_task, timeout=_DEFAULT_SERVER_STOP_TIMEOUT
                        )
                    except TimeoutError:
                        if on_warning is not None:
                            on_warning(
                                f"Timed out waiting for temporary package server ({repo_name}) to stop."
                            )
                        serve_task.cancel()

    task = asyncio.create_task(impl())
    try:
        await started_event.wait()
    except BaseException:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        raise
    return (task, cancel_event)


async def wait_for_package_server(
    process: asyncio.subprocess.Process, timeout: float, interval: float
) -> bool:
    """
    Waits for a package server to come online by periodically polling.
    Exits early if the given package serving process terminates.

    Args:
        process (Process): The package serving process. If it terminates early, this function
            automatically returns False.
        timeout (float): The maximum amount of time (in seconds) to wait for the package server.
        interval (float): The delay (in seconds) between each package server availability check.

    Returns:
        bool: Whether the package serving process is available within `timeout` seconds.
    """

    async def poll_is_running(interval: float) -> Literal[True]:
        while not await is_running():
            await asyncio.sleep(interval)
        return True

    async def wait_for_process(
        process: asyncio.subprocess.Process,
    ) -> Literal[False]:
        await process.wait()
        return False

    async def wait_for_timeout(timeout: float) -> Literal[False]:
        await asyncio.sleep(timeout)
        return False

    # This task represents the True condition: When `fx is-package-server-running` is True.
    poll_task = asyncio.create_task(poll_is_running(interval))

    # This task represents a False condition: When `process` terminates unexpectedly.
    process_task = asyncio.create_task(wait_for_process(process))

    # This task represents a False condition: When `timeout` has been reached.
    timeout_task = asyncio.create_task(wait_for_timeout(timeout))

    # Race the three tasks, we use the value of whichever task completes first.
    all_tasks = [poll_task, process_task, timeout_task]
    finished_task, _ = await asyncio.wait(
        all_tasks,
        return_when=asyncio.FIRST_COMPLETED,
    )

    is_serving = finished_task.pop().result() if finished_task else False

    # Cleanup other tasks.
    for task in all_tasks:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

    return is_serving


def is_package_repository_built() -> bool:
    """
    Checks whether the in-tree package repository is built.

    Returns:
        bool: Whether the in-tree package repository is built.
    """
    return (
        build_dir.get_build_directory()
        / "amber-files"
        / "repository"
        / "9.root.json"
    ).is_file()


def ephemeral_repository_name(
    prefix: str = "tmp",
    build_directory: str | os.PathLike[str] | None = None,
) -> str:
    """
    Returns deterministic ephemeral repository name for a build directory.

    Args:
        prefix (str): Tool-specific prefix for the repository name.
        build_directory (str | PathLike[str] | None): Build directory whose
            package repository is being served, or None to use the active build
            directory.

    Returns:
        str: Deterministic repository name prefixed with `<prefix>-`.

    Raises:
        build_dir.GetBuildDirectoryError: If `build_directory` is None and no
            active build directory can be determined.
    """
    out_dir = (
        build_directory
        if build_directory is not None
        else build_dir.get_build_directory()
    )
    repo_path = os.path.realpath(os.path.join(out_dir, "amber-files"))
    path_hash = hashlib.sha256(repo_path.encode("utf-8")).hexdigest()[:16]
    return f"{prefix}-{path_hash}"


async def get_arguments(name: str | None = None) -> tuple[str, ...]:
    """
    Gets the ffx command line arguments for starting a package server.

    Args:
        name (str | None): Optionally specify the main repository name for this server.

    Returns:
        tuple[str, ...]: A tuple of ffx arguments for package serving.
    """
    out_dir = build_dir.get_build_directory()
    repo_name = name or ephemeral_repository_name(build_directory=out_dir)
    ffx_default_port = await FfxCmd().start(
        "config",
        "get",
        "repository.server.default_port",
    )
    stdout = (await ffx_default_port.run_to_completion()).stdout
    try:
        port = int(stdout.strip(' \t\n\r"'))
    except ValueError:
        port = 8083

    # LINT.IfChange
    return (
        "repository",
        "server",
        "start",
        "--foreground",
        "--address",
        f"[::]:{port}",
        "--repository",
        repo_name,
        "--repo-path",
        str(out_dir / "amber-files"),
        "--trusted-root",
        str(out_dir / "amber-files" / "repository" / "9.root.json"),
        "--alias",
        "fuchsia.com",
        "--alias",
        "chromium.org",
    )
    # LINT.ThenChange(//tools/devshell/serve)


async def start(
    name: str | None = None, timeout: float = _DEFAULT_SERVER_STARTUP_TIMEOUT
) -> asyncio.subprocess.Process:
    """
    Starts a package server.

    Args:
        name (str | None): Optionally specify the main repository name for this server.
        timeout (float): The maximum amount of time (in seconds) to wait for the package server.

    Returns:
        Process: An asyncio Process instance for the package server.
    """
    if not is_package_repository_built():
        raise PackageServingException("The package repository is not built!")

    repo_name = name or ephemeral_repository_name()
    fx = FxCmd()
    ffx = FfxCmd()
    args = await get_arguments(repo_name)
    process = await asyncio.create_subprocess_exec(
        *fx.command_line(*ffx.command_line(*args)),
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )

    if await wait_for_package_server(
        process, timeout, _CHECK_SERVER_POLLING_INTERVAL
    ):
        return process

    errmsg: str
    if process.returncode is None:
        errmsg = f"The package server didn't start within {timeout}s."
        process.terminate()
        await process.wait()
        await stop(repo_name)
    else:
        errmsg = f"The package server exited with code {process.returncode}."
    raise PackageServingException(errmsg + f"\nCommand: {shlex.join(args)}")


async def stop(name: str | None = None) -> None:
    """
    Stops running package servers.

    Args:
        name (str | None): Optionally specify the repository name of the server to terminate.
            If omitted, all package servers will be stopped.
    """
    process = await FfxCmd().start(
        "repository",
        "server",
        "stop",
        *([name] if name else []),
    )
    await process.run_to_completion()


@asynccontextmanager
async def run_and_stop_on_exit() -> AsyncGenerator[None, None]:
    """
    Starts a package server for the duration of the context block, if one is not currently running,
    and stops and deregisters it on exit.
    """
    if await is_running():
        yield
        return

    try:
        repo_name = ephemeral_repository_name()
        print(
            "You do not seem to have a package server running. "
            + "A temporary one will be started for the duration of this execution."
        )
        server = await start(repo_name)
    except PackageServingException as e:
        raise PackageServingCLIException(
            f"Error: {e}\nError: Failed to start the temporary package server. "
            + "You may need to manually start one via `fx serve`."
        )

    try:
        yield
    finally:
        try:
            await deregister(repo_name)
        finally:
            server.terminate()
            await server.wait()

            # Sending SIGTERM doesn't seem to clean up child processes, so use `ffx` to stop the
            # package server we just started for good measure.
            await stop(repo_name)
