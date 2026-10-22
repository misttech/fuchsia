# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Provides methods for Host-(Fuchsia)Target interactions via ADB."""

import atexit
import contextlib
import glob
import logging
import os
import shlex
import shutil
import stat
import tarfile
import tempfile
import threading
import time
import types
from collections.abc import Iterator
from importlib import resources
from pathlib import Path

from honeydew import errors
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.adb.adb_server import AdbServer
from honeydew.transports.ffx import ffx
from honeydew.utils import common, host_shell

_ADB_PATH_ENV_VAR = "HONEYDEW_ADB_OVERRIDE"

# Default timeout for check_connection(). Set to 300 seconds because check_connection()
# waits until the device is booted completely (wait_for_boot_complete), which can take
# multiple minutes.
_DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS: float = 300.0

_BOOT_COMPLETED_GETPROP_TIMEOUT_SECS: float = 10.0

_DEFAULT_RUN_ATTEMPTS: int = 3

_LOGGER: logging.Logger = logging.getLogger(__name__)


def _extract_keys_tar(
    tar_path: str | Path,
) -> tempfile.TemporaryDirectory[str]:
    """Extracts an ADB vendor keys tarball into a new temporary directory."""
    temp_dir = tempfile.TemporaryDirectory()
    try:
        target_dir = Path(temp_dir.name).resolve()
        with tarfile.open(tar_path, "r") as tar:
            for member in tar.getmembers():
                if member.issym() or member.islnk():
                    raise adb_errors.AdbError(
                        f"Symbolic or hard links are not allowed in key tar: {member.name}"
                    )
                member_path = target_dir.joinpath(member.name).resolve()
                try:
                    member_path.relative_to(target_dir)
                except ValueError:
                    raise adb_errors.AdbError(
                        f"Unsafe member in tar file: {member.name}"
                    )
            if hasattr(tarfile, "data_filter"):
                tar.extractall(path=temp_dir.name, filter="data")
            else:
                tar.extractall(path=temp_dir.name)
        _LOGGER.info(
            "Extracted adb keys from tarball to %s",
            temp_dir.name,
        )
        return temp_dir
    except Exception:
        temp_dir.cleanup()
        raise


def _get_bundled_keys_tar() -> tempfile.TemporaryDirectory[str] | None:
    """Extracts bundled adb_keys.tar from honeydew.data if present."""
    try:
        from honeydew import data  # type: ignore[attr-defined]

        with resources.as_file(
            resources.files(data).joinpath("adb_keys.tar")
        ) as f:
            if f.exists():
                _LOGGER.info("Using ADB vendor keys path: %s", f)
                return _extract_keys_tar(f)
    except (ImportError, FileNotFoundError, AttributeError, TypeError):
        pass
    return None


def _resolve_vendor_keys_path(
    vendor_keys_path: str | None = None,
) -> tuple[str | None, tempfile.TemporaryDirectory[str] | None]:
    """Resolves the vendor keys path, extracting tar archives if needed.

    Checks in order:
    1. Explicit `vendor_keys_path` argument (directory or `.tar` file).
    2. `ADB_VENDOR_KEYS` environment variable (directory or `.tar` file).
    3. Bundled Python data resource (`honeydew.data/adb_keys.tar`), which is
       packaged at build time from the `honeydew_adb_keys_dir` GN build arg
       (defaults to `//third_party/android/platform/vendor/google/security/adb`
       if present, or can be overridden via
       `fx set ... --args='honeydew_adb_keys_dir="//path/to/keys"'`).

    If the resolved path is a `.tar` archive and exists, extracts it to a
    temporary directory.

    Returns:
        A tuple of (resolved_path, temp_directory_object).
    """
    temp_dir: tempfile.TemporaryDirectory[str] | None = None

    if vendor_keys_path is None:
        vendor_keys_path = os.environ.get("ADB_VENDOR_KEYS")

    if vendor_keys_path:
        if os.path.exists(vendor_keys_path):
            _LOGGER.info("Using ADB vendor keys path: %s", vendor_keys_path)
            if vendor_keys_path.endswith(".tar"):
                temp_dir = _extract_keys_tar(vendor_keys_path)
                vendor_keys_path = temp_dir.name
        else:
            _LOGGER.warning(
                "ADB vendor keys path %s does not exist; falling back to bundled keys.",
                vendor_keys_path,
            )
            vendor_keys_path = None

    if not vendor_keys_path:
        temp_dir = _get_bundled_keys_tar()
        if temp_dir:
            vendor_keys_path = temp_dir.name

    return vendor_keys_path, temp_dir


def _get_adb_binary() -> str:
    """Returns the path to the `adb` binary.

    Prefers resolving from environment variable `HONEYDEW_ADB_OVERRIDE` if
    provided; otherwise, extract from Python resource, set permissions to
    executable, and store on disk. If running outside the build system without
    data resources, falls back to `PATH`.

    Returns:
        Absolute path to `adb` binary.

    Raises:
        adb_errors.InitializationError: If ADB binary is not found.
    """

    bin_path: str | None = os.getenv(_ADB_PATH_ENV_VAR)
    if bin_path is not None:
        return bin_path

    try:
        from honeydew import data  # type: ignore[attr-defined,unused-ignore]

        bin_fd = tempfile.NamedTemporaryFile(suffix="adb", delete=False)
        bin_path = bin_fd.name
        bin_fd.close()
        with resources.as_file(resources.files(data).joinpath("adb")) as f:
            f.chmod(f.stat().st_mode | stat.S_IEXEC)
            shutil.copy2(f, bin_path)
        atexit.register(os.unlink, bin_path)
        return bin_path
    except (ImportError, FileNotFoundError, AttributeError, TypeError):
        pass

    bin_path = shutil.which("adb")
    if bin_path is not None:
        return bin_path

    raise adb_errors.InitializationError(
        "ADB binary was not found in Python data resources, in PATH, or via "
        f"`{_ADB_PATH_ENV_VAR}`."
    )


def _find_usb_device_path(target_serial: str | None = None) -> str | None:
    """Finds the sysfs device path for the given serial (or Google VID)."""
    for dev_path in glob.glob("/sys/bus/usb/devices/*"):
        try:
            if target_serial:
                serial_file = os.path.join(dev_path, "serial")
                if not os.path.exists(serial_file):
                    continue
                with open(serial_file, "r", encoding="utf-8") as f:
                    ser = f.read().strip()
                if ser == target_serial:
                    return dev_path
            else:
                vendor_file = os.path.join(dev_path, "idVendor")
                if not os.path.exists(vendor_file):
                    continue
                with open(vendor_file, "r", encoding="utf-8") as f:
                    vid = f.read().strip()
                if vid == "18d1":
                    return dev_path
        except OSError:
            pass
    return None


def _has_adb_interface(dev_path: str) -> bool:
    """Checks if the given sysfs USB device path exposes an ADB interface."""
    for intf_path in glob.glob(os.path.join(dev_path, "*:*.*/bInterfaceClass")):
        intf_dir = os.path.dirname(intf_path)
        try:
            with open(
                os.path.join(intf_dir, "bInterfaceClass"), "r", encoding="utf-8"
            ) as f:
                cls = f.read().strip()
            with open(
                os.path.join(intf_dir, "bInterfaceSubClass"),
                "r",
                encoding="utf-8",
            ) as f:
                subcls = f.read().strip()
            with open(
                os.path.join(intf_dir, "bInterfaceProtocol"),
                "r",
                encoding="utf-8",
            ) as f:
                proto = f.read().strip()

            if cls == "ff" and subcls == "42" and proto == "01":
                return True
        except OSError:
            pass
    return False


def _check_adb_sysfs(target_serial: str | None = None) -> bool:
    """Checks sysfs to see if there is an ADB interface for the given serial."""
    dev_path = _find_usb_device_path(target_serial)
    if dev_path:
        return _has_adb_interface(dev_path)
    return False


class _AdbRootContextManager(
    contextlib.AbstractContextManager[None],
    contextlib.AbstractAsyncContextManager[None],
):
    """Context manager for temporarily enabling root privileges via ADB."""

    def __init__(
        self,
        adb_transport: "Adb",
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        self._adb: Adb = adb_transport
        self._timeout: float | None = timeout
        self._attempts: int = attempts

    def __enter__(self) -> None:
        with self._adb._root_lock:
            if self._adb._root_ref_count == 0:
                if not self._adb.is_root:
                    self._adb.root(
                        timeout=self._timeout, attempts=self._attempts
                    )
                    self._adb._rooted_by_context = True
                else:
                    self._adb._rooted_by_context = False
            self._adb._root_ref_count += 1

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> None:
        with self._adb._root_lock:
            self._adb._root_ref_count -= 1
            if self._adb._root_ref_count == 0:
                if self._adb._rooted_by_context:
                    self._adb._rooted_by_context = False
                    self._adb.unroot(
                        timeout=self._timeout, attempts=self._attempts
                    )

    async def __aenter__(self) -> None:
        self.__enter__()

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> None:
        self.__exit__(exc_type, exc_val, exc_tb)


class Adb:
    """Provides methods for Host-(Fuchsia)Target interactions via ADB.

    Note: The USB detection logic (`verify_supported()`) relies on Linux-specific
    sysfs (`/sys/bus/usb/devices/*`) and is only supported on Linux hosts.

    Args:
        device_name: Fuchsia device name.
        serial_number: Target device serial number.
        ffx_transport: FFX transport instance used to restart wedged
            device-side adbd processes during recovery.
        run_isolated_server: Whether to run an isolated ADB server.
        output_dir: Directory where ADB server logs should be stored.
        vendor_keys_path: Path to custom vendor keys.

    Raises:
        NotSupportedError: If ADB transport is not supported on the device.
        InitializationError: If ADB binary is not found or isolated ADB server fails to start.
        AdbConnectionError: If ADB fails to connect to the device.
    """

    def __init__(
        self,
        device_name: str,
        serial_number: str,
        ffx_transport: ffx.FFX,
        run_isolated_server: bool = True,
        output_dir: Path | str | None = None,
        vendor_keys_path: str | None = None,
    ) -> None:
        self._device_name: str = device_name
        self._serial_number: str = serial_number
        self._ffx: ffx.FFX = ffx_transport
        self._run_isolated_server: bool = run_isolated_server
        self._output_dir: Path | None = Path(output_dir) if output_dir else None
        (
            self._vendor_keys_path,
            self._temp_vendor_keys_dir,
        ) = _resolve_vendor_keys_path(vendor_keys_path)
        self._adb_server: AdbServer | None = None
        self._is_root: bool = False
        self._is_su_root: bool = False
        self._cached_adbd_pid: str | None = None
        self._root_lock: threading.Lock = threading.Lock()
        self._root_ref_count: int = 0
        self._rooted_by_context: bool = False

        try:
            self.verify_supported()

            self._adb_binary: str = _get_adb_binary()

            if self._run_isolated_server:
                try:
                    _LOGGER.info(
                        "Starting isolated ADB server for %s", self._device_name
                    )
                    self._adb_server = AdbServer(
                        adb_binary_path=self._adb_binary,
                        serial_id=self._serial_number,
                        output_dir=self._output_dir,
                        vendor_keys_path=self._vendor_keys_path,
                    )
                    self._adb_server.start()
                except errors.HoneydewError as err:
                    raise adb_errors.InitializationError(
                        f"Failed to start isolated ADB server for {self._device_name}: {err}"
                    ) from err

            self.check_connection()
            # Cache the remote adbd PID upon initial connection so FFX can kill
            # a wedged adbd out-of-band if ADB later becomes unresponsive.
            self._cache_adbd_pid()
            atexit.register(self.close)
        except BaseException:
            self.close()
            raise

    def on_device_boot(self) -> None:
        """Takes actions after the Fuchsia device reboots.

        Resets cached root state (since Starnix restarts adbd as the shell user
        on boot), verifies the ADB connection is back online, and caches the PID
        of the newly spawned remote adbd process.
        """
        with self._root_lock:
            self._is_root = False
            self._is_su_root = False
            self._rooted_by_context = False
            self._root_ref_count = 0
            self._cached_adbd_pid = None
        self.check_connection()
        # A device reboot spawns a fresh adbd process in Starnix with a new PID;
        # update the cached PID so FFX can kill it if ADB later wedges.
        self._cache_adbd_pid()

    def _build_adb_cmd(
        self, cmd: list[str], include_serial: bool = True
    ) -> list[str]:
        """Constructs the full ADB command line list."""
        adb_cmd = [self._adb_binary]
        if self._adb_server:
            adb_cmd.extend(
                [
                    "-H",
                    self._adb_server.host(),
                    "-P",
                    str(self._adb_server.port()),
                ]
            )
        if include_serial:
            adb_cmd.extend(["-s", self._serial_number])

        # TODO(b/564946370): Remove once 'root' operations are stable. When
        # using a USB-based connection, use 'su root' instead of 'root', which
        # prevents flapping the ADBD instance on-device.
        if self._is_su_root and "shell" in cmd:
            shell_index = cmd.index("shell")
            # Make a copy of the command line passed in. The list type is mutable, so if the
            # same list is used to run a command in a loop, 'su','root' will appear multiple times.
            cmd = cmd.copy()
            cmd.insert(shell_index + 1, "root")
            cmd.insert(shell_index + 1, "su")
        adb_cmd.extend(cmd)
        return adb_cmd

    def _get_command_env(self) -> dict[str, str] | None:
        """Returns the subprocess environment dictionary for ADB commands."""
        env: dict[str, str] | None = None
        if self._vendor_keys_path:
            env = os.environ.copy()
            env["ADB_VENDOR_KEYS"] = self._vendor_keys_path
        elif "ADB_VENDOR_KEYS" in os.environ:
            env = os.environ.copy()
            env.pop("ADB_VENDOR_KEYS", None)
        return env

    def _cache_adbd_pid(self) -> None:
        """Caches the PID of the remote adbd process on the device."""
        try:
            output = host_shell.run(
                cmd=self._build_adb_cmd(
                    ["shell", "pidof", "adbd"], include_serial=True
                ),
                capture_output=True,
                capture_error_in_output=True,
                timeout=10.0,
                env=self._get_command_env(),
            )
            pid = (output or "").strip()
            if pid and pid.isdigit():
                self._cached_adbd_pid = pid
                _LOGGER.debug(
                    "Cached remote adbd PID for %s: %s",
                    self._device_name,
                    self._cached_adbd_pid,
                )
        except errors.HoneydewError as err:
            _LOGGER.debug(
                "Failed to cache remote adbd PID for %s: %s",
                self._device_name,
                err,
            )

    def _restart_adbd(self) -> None:
        """Kills the cached remote adbd process on the device via FFX."""
        if not self._cached_adbd_pid:
            _LOGGER.debug(
                "Not restarting remote adbd on %s (cached_adbd_pid is None).",
                self._device_name,
            )
            return
        _LOGGER.warning(
            "Restarting remote adbd on %s (pid=%s) via FFX...",
            self._device_name,
            self._cached_adbd_pid,
        )
        try:
            self._ffx.run(
                ["starnix", "kill", "-p", self._cached_adbd_pid, "-s", "9"],
                timeout=10.0,
            )
        except errors.HoneydewError as err:
            _LOGGER.warning(
                "Failed to kill remote adbd (pid=%s) on %s via FFX: %s",
                self._cached_adbd_pid,
                self._device_name,
                err,
            )
        finally:
            self._cached_adbd_pid = None

    def _reconnect_offline(self) -> None:
        """Runs `adb reconnect offline` to recover an offline ADB connection."""
        _LOGGER.warning(
            "Device %s is offline. Attempting `adb reconnect offline`...",
            self._device_name,
        )
        try:
            host_shell.run(
                cmd=self._build_adb_cmd(
                    ["reconnect", "offline"], include_serial=False
                ),
                capture_output=True,
                capture_error_in_output=True,
                timeout=10.0,
                env=self._get_command_env(),
            )
        except errors.HoneydewError as err:
            _LOGGER.warning(
                "`adb reconnect offline` failed for %s: %s",
                self._device_name,
                err,
            )

    def _recover_adb_server(self) -> None:
        """Restarts remote adbd (if cached) and isolated host ADB server, restoring root if needed."""
        assert self._adb_server is not None
        self._restart_adbd()
        self._adb_server.restart()
        time.sleep(10)
        if self._is_root:
            _LOGGER.info(
                "Restoring root privileges on %s after ADB recovery...",
                self._device_name,
            )
            try:
                for root_cmd in (["root"], ["wait-for-device"]):
                    host_shell.run(
                        cmd=self._build_adb_cmd(root_cmd, include_serial=True),
                        capture_output=True,
                        capture_error_in_output=True,
                        timeout=30.0,
                        env=self._get_command_env(),
                    )
            except errors.HoneydewError as err:
                _LOGGER.warning(
                    "Failed to restore root privileges on %s after ADB recovery: %s",
                    self._device_name,
                    err,
                )
        # Cache the newly spawned remote adbd PID after killing the old adbd
        # process and/or restarting adbd as root.
        self._cache_adbd_pid()

    def verify_supported(self) -> None:
        """Verifies that ADB transport is supported by the Fuchsia device.

        This method should be called in `__init__()` so that if ADB is used
        on a Fuchsia device that does not support it, it will raise
        NotSupportedError.

        Raises:
            NotSupportedError: If ADB transport is not supported.
        """
        if not _check_adb_sysfs(self._serial_number):
            raise errors.NotSupportedError(
                f"ADB transport is not supported on {self._device_name}"
            )

    def _get_remaining_timeout(
        self,
        start_time: float,
        timeout: float | None,
        operation: str | None = None,
    ) -> float | None:
        """Calculates the remaining timeout after an operation.

        Args:
            start_time: The start time (from `time.time()`) of the operation.
            timeout: The original total timeout in seconds, or None.
            operation: Name of the operation being performed.

        Returns:
            The remaining timeout in seconds, or None if `timeout` is None.

        Raises:
            AdbTimeoutError: If the timeout has already expired.
        """
        if timeout is None:
            return None
        elapsed: float = time.time() - start_time
        remaining_timeout: float = timeout - elapsed
        if remaining_timeout <= 0:
            op_desc = f" during '{operation}'" if operation else ""
            raise adb_errors.AdbTimeoutError(
                f"Timed out after {timeout}s{op_desc} on {self._device_name}"
            )
        return remaining_timeout

    def check_connection(
        self,
        timeout: float | None = _DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Checks the ADB connection from host to Fuchsia device.

        Args:
            timeout: Maximum amount of time in seconds to wait for connection
                and boot complete. Defaults to 300.0 seconds.
            attempts: Maximum number of attempts to run the `wait-for-device`
                command. Defaults to 3.

        Raises:
            AdbUnauthorizedError: If the device is unauthorized for ADB connections.
            AdbConnectionError: If ADB fails to connect to the device or wait for
                boot complete.
        """
        try:
            _LOGGER.info(
                "Checking ADB connection from host to %s...",
                self._device_name,
            )
            start_time: float = time.time()
            self.run(["wait-for-device"], timeout=timeout, attempts=attempts)

            remaining_timeout: float | None = self._get_remaining_timeout(
                start_time=start_time,
                timeout=timeout,
                operation="check_connection",
            )

            self.wait_for_boot_complete(timeout=remaining_timeout)
            _LOGGER.info(
                "ADB completed the connection check from host to %s.",
                self._device_name,
            )
        except adb_errors.AdbConnectionError:
            raise
        except errors.HoneydewError as err:
            raise adb_errors.AdbConnectionError(
                f"ADB connection check failed for {self._device_name} with err: {err}"
            ) from err

    def run(
        self,
        cmd: list[str],
        timeout: float | None = None,
        include_serial: bool = True,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> str:
        """Runs an ADB command and returns the output.

        Args:
            cmd: ADB command as a list of strings (excluding 'adb' and '-s <serial>' if include_serial=True).
            timeout: Maximum amount of time in seconds to wait for the command to finish across all
                attempts. Defaults to None (no time limit), which is recommended to avoid flakiness
                on slow or overloaded test environments. Only pass a timeout when explicitly
                required or for commands expected to fail fast.
            include_serial: Whether to include '-s <serial_number>' in the command.
                Defaults to True. Should be set to False for commands like 'adb devices'.
            attempts: Maximum number of attempts to run the command. Defaults to 3.

        Returns:
            The combined stdout and stderr of the command.

        Raises:
            adb_errors.AdbUnauthorizedError: If the device is unauthorized for ADB commands.
            adb_errors.AdbTimeoutError: If the command times out.
            adb_errors.AdbCommandError: If the command fails.
        """
        timeout_per_attempt: float | None = None
        if timeout is not None:
            timeout_per_attempt = timeout / attempts
            _LOGGER.info(
                "Timeout of %ss (%ss per attempt) is set for ADB command '%s'. "
                "Note that timeouts can cause flakiness on overloaded test environments.",
                timeout,
                timeout_per_attempt,
                " ".join(cmd),
            )

        attempt = 1
        while True:
            # Construct adb_cmd inside the loop to ensure we use the updated port if restarted
            adb_cmd = self._build_adb_cmd(cmd, include_serial=include_serial)

            _LOGGER.debug(
                "Running ADB command (attempt %d/%d): %s",
                attempt,
                attempts,
                adb_cmd,
            )
            env = self._get_command_env()

            try:
                output: str = (
                    host_shell.run(
                        cmd=adb_cmd,
                        capture_output=True,
                        capture_error_in_output=True,
                        timeout=timeout_per_attempt,
                        env=env,
                    )
                    or ""
                )
                if self._adb_server:
                    self._adb_server.reset_restart_count()
                return output

            except errors.HoneydewTimeoutError as err:
                if attempt < attempts and self._adb_server:
                    _LOGGER.warning(
                        "ADB command timed out. "
                        "Attempting to restart isolated ADB server and retry..."
                    )
                    self._recover_adb_server()
                    attempt += 1
                    continue
                raise adb_errors.AdbTimeoutError(
                    f"ADB command '{adb_cmd}' timed out after {timeout} seconds"
                ) from err

            except errors.HostCmdError as err:
                err_msg_lower = str(err).lower()
                if "unauthorized" in err_msg_lower:
                    raise adb_errors.AdbUnauthorizedError(
                        f"Device '{self._device_name}' ({self._serial_number}) is unauthorized. "
                        "Ensure valid ADB vendor keys are bundled or configured via vendor_keys_path / ADB_VENDOR_KEYS. "
                        f"Original error: {err}"
                    ) from err

                is_retriable = any(
                    x in err_msg_lower
                    for x in [
                        "offline",
                        "not found",
                        "connection refused",
                        "cannot connect",
                    ]
                )
                if is_retriable and attempt < attempts:
                    if "offline" in err_msg_lower and attempt == 1:
                        self._reconnect_offline()
                        time.sleep(2)
                        attempt += 1
                        continue
                    if self._adb_server:
                        _LOGGER.warning(
                            "ADB command failed with connection error: %s. "
                            "Attempting to restart isolated ADB server and retry...",
                            str(err),
                        )
                        self._recover_adb_server()
                        attempt += 1
                        continue
                raise adb_errors.AdbCommandError(str(err)) from err

            except errors.HoneydewError as err:
                raise adb_errors.AdbCommandError(
                    f"Failed to run ADB command '{adb_cmd}': {err}"
                ) from err

    @property
    def is_root(self) -> bool:
        """Whether the ADB daemon on the device is currently running as root."""
        return self._is_root

    @property
    def is_su_root(self) -> bool:
        """Whether su_root mode is currently enabled on the ADB transport."""
        return self._is_su_root

    def root(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Restarts ADB daemon on device as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.

        Raises:
            AdbTimeoutError: If the command times out.
            AdbCommandError: If the command fails.
        """
        _LOGGER.info("Enabling root-privileges on %s.", self._device_name)
        start_time: float = time.time()
        self.run(["root"], timeout=timeout, attempts=attempts)
        remaining_timeout: float | None = self._get_remaining_timeout(
            start_time=start_time, timeout=timeout, operation="root"
        )
        self.run(
            ["wait-for-device"], timeout=remaining_timeout, attempts=attempts
        )
        self._is_root = True
        # `adb root` terminates the running adbd process and spawns a new adbd
        # process as root (UID 0) with a new PID; update the cached PID.
        self._cache_adbd_pid()

    def unroot(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Restarts ADB daemon on device as the shell user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.

        Raises:
            AdbTimeoutError: If the command times out.
            AdbCommandError: If the command fails.
        """
        _LOGGER.info("Disabling root-privileges on %s.", self._device_name)
        start_time: float = time.time()
        self.run(["unroot"], timeout=timeout, attempts=attempts)
        remaining_timeout: float | None = self._get_remaining_timeout(
            start_time=start_time, timeout=timeout, operation="unroot"
        )
        self.run(
            ["wait-for-device"], timeout=remaining_timeout, attempts=attempts
        )
        self._is_root = False
        self._rooted_by_context = False
        # `adb unroot` terminates the running adbd process and spawns a new adbd
        # process as shell (UID 2000) with a new PID; update the cached PID.
        self._cache_adbd_pid()

    def use_adb_root(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> _AdbRootContextManager:
        """Temporarily runs adb as root within a context.

        Does not attempt to root/unroot if ADB daemon is already running as the root user.
        Supports both synchronous (`with`) and asynchronous (`async with`) contexts.

        Usage:
        ```
        with self.use_adb_root():
            ... do something ...
        ```
        or in async functions:
        ```
        async with self.use_adb_root():
            ... do something ...
        ```

        Args:
            timeout: Maximum amount of time in seconds to wait for root/unroot commands.
            attempts: Maximum number of attempts to run the root/unroot commands. Defaults to 3.

        Returns:
            A context manager enabling root on enter and restoring previous root state on exit.
        """
        return _AdbRootContextManager(self, timeout=timeout, attempts=attempts)

    @contextlib.contextmanager
    def use_su_root(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> Iterator[None]:
        """Temporarily enables running adb as root within a context using su_root.

        Does not attempt to root/unroot if ADB daemon is already running as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for root/unroot commands.
            attempts: Maximum number of attempts to run the root/unroot commands. Defaults to 3.

        Usage:
        ```
        with self.use_su_root():
            ... do something ...
        ```
        """
        adbd_was_root = self._is_su_root
        if not adbd_was_root:
            self.su_root(timeout=timeout, attempts=attempts)

        try:
            yield
        finally:
            if not adbd_was_root:
                self.su_unroot(timeout=timeout, attempts=attempts)

    def su_root(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Restarts ADB daemon on device as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.
        """
        _LOGGER.info("Enabling root-privileges on %s.", self._device_name)
        # When using a USB-based connection, use 'su root' instead of 'root',
        # which prevents flapping the ADBD instance on-device. This is
        # handled internally by _build_adb_cmd.
        _LOGGER.debug("Using 'su root' instead of 'root'.")
        self._is_su_root = True

    def su_unroot(
        self,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Restarts ADB daemon on device as the shell user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.
        """
        _LOGGER.info("Disabling root-privileges on %s.", self._device_name)
        _LOGGER.debug("No longer using 'su root'.")
        self._is_su_root = False

    def setprop(
        self,
        prop_name: str,
        value: str,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> None:
        """Sets a system property on the device via `adb shell setprop <prop_name> <value>`.

        Args:
            prop_name: Name of the system property to set.
            value: Value to set the system property to.
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.

        Raises:
            AdbCommandError: If the command fails or times out.
        """
        _LOGGER.debug(
            "Setting property '%s' = '%s' on %s",
            prop_name,
            value,
            self._device_name,
        )
        self.run(
            ["shell", "setprop", shlex.quote(prop_name), shlex.quote(value)],
            timeout=timeout,
            attempts=attempts,
        )

    def getprop(
        self,
        prop_name: str,
        timeout: float | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
    ) -> str:
        """Gets a system property from the device via `adb shell getprop <prop_name>`.

        Args:
            prop_name: Name of the system property to get.
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to 3.

        Returns:
            The value of the property stripped of whitespace.

        Raises:
            AdbCommandError: If the command fails or times out.
        """
        value = self.run(
            ["shell", "getprop", shlex.quote(prop_name)],
            timeout=timeout,
            attempts=attempts,
        ).strip()
        _LOGGER.debug(
            "Got property '%s' = '%s' on %s",
            prop_name,
            value,
            self._device_name,
        )
        return value

    def wait_for_boot_complete(
        self, timeout: float | None = None, poll_interval: float = 1.0
    ) -> None:
        """Waits until `sys.boot_completed` returns '1'.

        Args:
            timeout: Maximum amount of time in seconds to wait for boot to complete.
                Defaults to None (wait indefinitely).
            poll_interval: Interval in seconds to wait between polling attempts.
                Defaults to 1.0 second.

        Raises:
            AdbTimeoutError: If boot does not complete within the timeout period.
        """

        def _is_boot_completed() -> bool:
            try:
                return (
                    self.getprop(
                        "sys.boot_completed",
                        timeout=_BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
                        attempts=1,
                    )
                    == "1"
                )
            except adb_errors.AdbError as err:
                _LOGGER.debug(
                    "Error querying sys.boot_completed on %s during boot: %s",
                    self._device_name,
                    err,
                )
                return False

        try:
            common.wait_for_state_sync(
                state_fn=_is_boot_completed,
                expected_state=True,
                timeout=timeout,
                wait_time=poll_interval,
            )
        except errors.HoneydewTimeoutError as err:
            raise adb_errors.AdbTimeoutError(
                f"Timed out after {timeout} seconds waiting for {self._device_name} "
                "to complete boot (sys.boot_completed != '1')."
            ) from err

    def close(self) -> None:
        """Cleans up the ADB transport.

        Safe to call multiple times.
        """
        # Unregister so an explicit close() is not repeated at process exit and
        # the atexit registry no longer keeps this instance alive.
        atexit.unregister(self.close)
        if self._adb_server:
            _LOGGER.info(
                "Stopping isolated ADB server for %s", self._device_name
            )
            self._adb_server.stop()
            self._adb_server = None
        if self._temp_vendor_keys_dir:
            self._temp_vendor_keys_dir.cleanup()
            self._temp_vendor_keys_dir = None
