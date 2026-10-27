# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FFX transport for Host-Target interactions."""

import ipaddress
import json
import logging
import math
import subprocess
import time
from pathlib import Path
from typing import Any

from honeydew import errors
from honeydew.affordances_capable import FuchsiaDeviceAddressChange
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx.types import (
    MachineFormat,
    TargetInfoData,
)
from honeydew.typing import custom_types
from honeydew.utils import host_shell, properties

_FFX_BINARY: str = "ffx"

_FFX_CMDS: dict[str, list[str]] = {
    "TARGET_SHOW": ["target", "show"],
    "TARGET_ADDRESS": [
        "target",
        "list",
        "--no-probe",  # do not connect to targets
    ],
    "TARGET_LIST": ["target", "list"],
    "TARGET_WAIT": ["target", "wait", "--timeout"],
    "TARGET_WAIT_DOWN": ["target", "wait", "--down", "--timeout", "0"],
    "TEST_RUN": ["test", "run"],
    "TARGET_SSH": ["target", "ssh"],
    "TARGET_STATUS": ["target", "status"],
    "MONITOR_INTENTIONAL_DISCONNECT": ["monitor", "intentional-disconnect"],
    "MONITOR_CONFIG_GET": [
        "config",
        "get",
        "monitor.pid_file",
    ],
}

_LOGGER: logging.Logger = logging.getLogger(__name__)

# LINT.IfChange(timeout_reaching_target)
_DEVICE_NOT_CONNECTED: str = "Timeout attempting to reach target"
# LINT.ThenChange(
#     //src/developer/ffx/lib/target/src/lib.rs:timeout_reaching_target,
#     //tools/lib/ffxutil/constants/constants.go:timeout_reaching_target,
#     //tools/testing/tefmocheck/check-configs.yaml:timeout_reaching_target
# )

# Timeout for `ffx target status` when collecting diagnostic logs after a command failure.
# 30 seconds provides sufficient headroom for FFX discovery and SSH connection attempts
# when the target is reachable, while preventing unbounded hangs when the target is offline.
_TARGET_STATUS_TIMEOUT_SECS: float = 30.0

# Timeout for `ffx monitor intentional-disconnect`.
# Since notifying ffx monitor is best-effort (ffx monitor may not even be running),
# cap the command execution time so it cannot hang the test workflow.
_NOTIFY_INTENTIONAL_DISCONNECT_TIMEOUT_SECS: float = 15.0

# Extra host-side time allowed for `ffx target wait --timeout N` to exit on
# its own before the process is killed.
_TARGET_WAIT_HOST_SLACK_SEC: float = 10.0

# Maximum time and polling interval when waiting for a specific target address
# family (IP or USB) to appear in `ffx target list` (e.g. when USB enumerates
# before mDNS/IP is ready after a reboot).
_RESOLVE_TARGET_ADDRESS_TIMEOUT_SECS: float = 30.0
_RESOLVE_TARGET_ADDRESS_POLL_INTERVAL_SECS: float = 2.0


class FFX:
    """Provides methods for Host-(Fuchsia)Target interactions via FFX.

    Args:
        target_name: Human-readable name of the target.
        config_data: Configuration associated with FFX.
        target_addr: Fuchsia device target address (IP or USB).
        device_address_change: Object that implements FuchsiaDeviceAddressChange to handle Fuchsia
            device address changes.

    Raises:
        ValueError: If target_addr is provided and device_address_change is not provided.
        FfxConnectionError: In case of failed to check FFX connection.
        FfxCommandError: In case of failure.
    """

    def __init__(
        self,
        target_name: str,
        config_data: ffx_config.FfxConfigData,
        target_addr: custom_types.TargetAddr | None = None,
        device_address_change: FuchsiaDeviceAddressChange | None = None,
    ) -> None:
        self._config_data: ffx_config.FfxConfigData = config_data
        self._target_name: str = target_name
        self._target_addr: custom_types.TargetAddr | None = target_addr

        if self._target_addr is not None and device_address_change is None:
            raise ValueError(
                "Pass 'device_address_change' argument also when 'target_addr' is passed"
            )

        self._device_address_change: FuchsiaDeviceAddressChange | None = (
            device_address_change
        )
        if self._device_address_change:
            self._device_address_change.register_for_on_device_address_change(
                fn=self._on_device_address_change
            )

        self.check_connection()

    @properties.PersistentProperty
    def config(self) -> ffx_config.FfxConfigData:
        """Returns the FFX configuration associated with this instance of FFX
        object.

        Returns:
            FFXConfig
        """
        return self._config_data

    @properties.PersistentProperty
    def serial_number(self) -> str | None:
        """Returns the device serial number from FFX target show.

        Returns:
            Serial number if available, else None.
        """
        target_info = self.get_target_info_from_target_show()
        return target_info.device.serial_number

    def check_connection(self, timeout: float | None = None) -> None:
        """Checks the FFX connection from host to Fuchsia device.

        Args:
            timeout: Maximum time in seconds to wait for the connection. If
                None (default), waits indefinitely.

        Raises:
            FfxConnectionError
        """
        try:
            self.wait_for_rcs_connection(timeout=timeout)
        except errors.HoneydewError as err:
            raise ffx_errors.FfxConnectionError(
                f"FFX connection check failed for {self._target_name} with err: {err}"
            ) from err

    def get_target_info_from_target_show(self) -> TargetInfoData:
        """Executed and returns the output of `ffx -t {target} target show`.

        Returns:
            Output of `ffx -t {target} target show`.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        cmd: list[str] = _FFX_CMDS["TARGET_SHOW"]
        output: str = self.run(cmd=cmd)

        target_info = TargetInfoData(**json.loads(output))
        _LOGGER.debug("`%s` returned: %s", " ".join(cmd), target_info)

        return target_info

    def get_target_info_from_target_list(self) -> dict[str, Any]:
        """Executed and returns the output of
        `ffx --machine json target list <target>`.

        Returns:
            Output of `ffx --machine json target list <target>`.

        Raises:
            FfxCommandError: In case of FFX command failure.
        """
        if self._target_addr is not None:
            target = str(self._target_addr)
        else:
            target = self._target_name

        cmd: list[str] = _FFX_CMDS["TARGET_LIST"] + [target]
        output: str = self.run(
            cmd=cmd,
            include_target=False,
        )

        target_info_from_target_list: list[dict[str, Any]] = json.loads(output)
        _LOGGER.debug(
            "`%s` returned: %s", " ".join(cmd), target_info_from_target_list
        )

        if len(target_info_from_target_list) == 1:
            return target_info_from_target_list[0]
        else:
            raise ffx_errors.FfxCommandError(
                f"'{target}' is not connected to host"
            )

    def get_target_name(self) -> str:
        """Returns the target name.

        Returns:
            Target name.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        ffx_target_show_info: TargetInfoData = (
            self.get_target_info_from_target_show()
        )
        return ffx_target_show_info.target.name

    def _resolve_target_address(self) -> None:
        """Resolves the target using 'ffx target list' and caches it."""
        cmd: list[str] = _FFX_CMDS["TARGET_ADDRESS"] + [self._target_name]
        expected_type: str | None = None
        if isinstance(self._target_addr, custom_types.IpPort):
            expected_type = "Ip"
        elif isinstance(self._target_addr, custom_types.UsbAddress):
            expected_type = "Usb"

        deadline: float = (
            time.monotonic() + _RESOLVE_TARGET_ADDRESS_TIMEOUT_SECS
        )
        address_obj: dict[str, Any] | None = None

        while True:
            # Skip the `ffx target status` triage on failure: it needs the target
            # address that is being resolved here, so running it would call back
            # into this method and recurse indefinitely.
            output: str = self.run(
                cmd=cmd, include_target=False, log_status_on_failure=False
            )
            targets = json.loads(output)
            if not targets:
                raise ffx_errors.FfxCommandError(
                    f"Target '{self._target_name}' not found in 'ffx target list'"
                )
            target = targets[0]
            if not target.get("addresses"):
                raise ffx_errors.FfxCommandError(
                    f"No addresses found for target '{self._target_name}'"
                )
            addresses: list[dict[str, Any]] = target["addresses"]

            if expected_type is None:
                address_obj = addresses[0]
                break

            address_obj = next(
                (
                    addr
                    for addr in addresses
                    if addr.get("type") == expected_type
                ),
                None,
            )
            if address_obj is not None:
                break

            if time.monotonic() >= deadline:
                raise ffx_errors.FfxCommandError(
                    f"No '{expected_type}' address found for target "
                    f"'{self._target_name}' in {addresses}"
                )

            _LOGGER.debug(
                "Target '%s' does not yet have a '%s' address in %s; retrying...",
                self._target_name,
                expected_type,
                addresses,
            )
            time.sleep(_RESOLVE_TARGET_ADDRESS_POLL_INTERVAL_SECS)

        try:
            if address_obj.get("type") == "Ip":
                ssh_ip = address_obj.get("ip")
                if ssh_ip is None:
                    raise ValueError(f"Missing ip address in {address_obj}")
                ssh_port = address_obj.get("ssh_port")
                if ssh_port == 0:
                    ssh_port = None
                self._target_addr = custom_types.IpPort(
                    ip=ipaddress.ip_address(ssh_ip), port=ssh_port
                )
            elif address_obj.get("type") == "Usb":
                cid = address_obj.get("cid")
                if not isinstance(cid, int) or isinstance(cid, bool):
                    raise ValueError(
                        f"USB address has invalid or missing 'cid': {address_obj}"
                    )
                self._target_addr = custom_types.UsbAddress(target_id=cid)
            else:
                raise ValueError(
                    f"Unable to create TargetAddr for {address_obj}"
                )
        except ValueError as e:
            raise ffx_errors.FfxCommandError(
                f"Failed to parse target address from FFX: {e}"
            ) from e

    def get_target_address(
        self, refresh: bool = False
    ) -> custom_types.TargetAddr:
        """Returns the TargetAddr. Resolves and caches it if not already cached or if refresh is True."""
        if self._target_addr is None or refresh:
            self._resolve_target_address()
        assert self._target_addr is not None
        return self._target_addr

    # TODO: Remove resolve_target_address() once //vendor/google code is
    # updated to use get_target_address(refresh=True).
    def resolve_target_address(self) -> None:
        """Resolves the target using 'ffx target list' and caches it."""
        self._resolve_target_address()

    # TODO: Remove get_target_ssh_address() once //vendor/google code is
    # updated to use get_target_address().
    def get_target_ssh_address(self) -> custom_types.TargetSshAddress | None:
        """Returns the target's ssh ip address and port information.

        Returns:
            (Target SSH IP Address, Target SSH Port) if address is an IP, None otherwise.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        addr = self.get_target_address()
        if isinstance(addr, custom_types.IpPort):
            return custom_types.TargetSshAddress(ip=addr.ip, port=addr.port)
        return None

    def get_target_board(self) -> str:
        """Returns the target's board.

        Returns:
            Target's board.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        target_show_info: TargetInfoData = (
            self.get_target_info_from_target_show()
        )
        return (
            target_show_info.build.board if target_show_info.build.board else ""
        )

    def get_target_product(self) -> str:
        """Returns the target's product.

        Returns:
            Target's product.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        target_show_info: TargetInfoData = (
            self.get_target_info_from_target_show()
        )
        return (
            target_show_info.build.product
            if target_show_info.build.product
            else ""
        )

    def get_target_status(self) -> str:
        """Returns FFX target status.

        Returns:
            Output of FFX command when capture_output is set to True, otherwise
            an empty string.

        Raises:
            FfxTargetStatusError: In case of HostCmdError.
        """
        cmd: list[str] = _FFX_CMDS["TARGET_STATUS"]
        ffx_cmd = self.generate_ffx_cmd(
            cmd=cmd,
            include_target=True,
        )
        # Use host_shell.run() directly instead of self.run() because
        # get_target_status() is invoked inside self.run()'s exception handler
        # for diagnostic triage, and needs to raise FfxTargetStatusError rather
        # than triggering recursive triage or target connection error translation.
        try:
            return (
                host_shell.run(
                    cmd=ffx_cmd,
                    capture_output=True,
                    log_output=False,
                    timeout=_TARGET_STATUS_TIMEOUT_SECS,
                )
                or ""
            )
        except errors.HostCmdError as err:
            raise ffx_errors.FfxTargetStatusError(err) from err

    def notify_intentional_disconnect(self) -> None:
        """Best-effort notification to the FFX monitor of an upcoming intentional disconnect.

        Since `ffx monitor` may be running in infra even when `FFXWithMonitor` is not used,
        this method lives on the base `FFX` class. Any `HoneydewError` (including timeout or
        command failure when monitor is not running) is logged at DEBUG level and suppressed.
        """
        if not self._check_running_monitor():
            _LOGGER.debug(
                "FFX monitor is not running; skipping intentional disconnect notification."
            )
            return

        cmd = _FFX_CMDS["MONITOR_INTENTIONAL_DISCONNECT"][:]
        cmd.extend(["--nodename", self._target_name])

        try:
            self.run(
                cmd=cmd,
                include_target=False,
                log_status_on_failure=False,
                timeout=_NOTIFY_INTENTIONAL_DISCONNECT_TIMEOUT_SECS,
            )
        except errors.HoneydewError as err:
            _LOGGER.debug(
                "Failed to notify intentional disconnect to FFX monitor: %s",
                err,
            )

    def _check_running_monitor(self) -> bool:
        """Best-effort check for whether there is a running FFX monitor.

        Returns:
            True if a monitor is running, False otherwise (including if the command fails).
        """
        cmd: list[str] = _FFX_CMDS["MONITOR_CONFIG_GET"]
        # "ffx config get" does not support JSON.
        # `include_target=False` is used because `monitor.pid_file` is a host-level
        # configuration resolved via `shared_data` for the single host monitor process
        # that tracks all connected targets, and omitting `-t <target>` avoids
        # unnecessary target address resolution.
        ffx_cmd: list[str] = self.generate_ffx_cmd(
            cmd=cmd,
            include_target=False,
            machine=MachineFormat.RAW,
        )
        # Use host_shell.run() directly instead of self.run() because this is a
        # best-effort, host-local config check that does not interact with the
        # target and should bypass self.run()'s target error handling and failure
        # triage.
        try:
            output: str = (host_shell.run(cmd=ffx_cmd) or "").strip().strip('"')
        except errors.HoneydewError as err:
            _LOGGER.debug(
                "Failed to check if FFX monitor is running: %s",
                err,
            )
            return False

        if not output:
            return False

        pid_file_exists: bool = Path(output).exists()
        _LOGGER.debug(
            "Fetched config: `%s` returned: %s path exists: `%s`",
            " ".join(cmd),
            output,
            pid_file_exists,
        )

        # If the pid_path exists, it means there is a running monitor.
        return pid_file_exists

    def run(
        self,
        cmd: list[str],
        timeout: float | None = None,
        capture_output: bool = True,
        log_output: bool = True,
        include_target: bool = True,
        machine: MachineFormat = MachineFormat.JSON,
        log_status_on_failure: bool = True,
        disable_controlmaster: bool = False,
        config_overrides: dict[str, Any] | None = None,
    ) -> str:
        """Runs an FFX command.

        Args:
            cmd: FFX command to run.
            timeout: Timeout to wait for the ffx command to return. By default,
                timeout is not set.
            capture_output: When True, the stdout/err from the command will be
                captured and returned. When False, the output of the command
                will be streamed to stdout/err accordingly and it won't be
                returned. Defaults to True.
            log_output: When True, logs the output in DEBUG level. Callers
                may set this to False when expecting particularly large
                or spammy output.
            include_target: If set to True, `ffx -t {target} {cmd}` will be run.
                Otherwise, `ffx {cmd}` will be run.
            machine: Specifies the machine format used for the ffx command (defaults
                to "json")
            log_status_on_failure: Whether to run diagnostic triage ('ffx target status')
                if the command fails or times out. Defaults to True.
            disable_controlmaster: boolean value of controlmaster.
            config_overrides: FFX configuration overrides.

        Returns:
            Output of FFX command when capture_output is set to True, otherwise
            an empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxTimeoutError: In case of FFX command timeout.
            FfxCommandError: In case of other FFX command failure.
        """
        ffx_cmd: list[str] = self.generate_ffx_cmd(
            cmd=cmd,
            include_target=include_target,
            machine=machine,
            disable_controlmaster=disable_controlmaster,
            config_overrides=config_overrides,
        )
        try:
            # TODO(b/484362368): when machine == `JSON`, we should parse the output
            # with json.loads() before returning
            return (
                host_shell.run(
                    cmd=ffx_cmd,
                    capture_output=capture_output,
                    log_output=log_output,
                    timeout=timeout,
                )
                or ""
            )
        except (errors.HostCmdError, errors.HoneydewTimeoutError) as err:
            if log_status_on_failure:
                try:
                    output: str = self.get_target_status()
                    _LOGGER.info(
                        "FFX Triage: State captured after command failure (ffx target status):\n%s",
                        output,
                    )
                except errors.HoneydewError as e:
                    _LOGGER.debug(
                        "Failed to execute diagnostic 'ffx target status' on target %s: %s",
                        self._target_name,
                        e,
                    )

            if isinstance(err, errors.HostCmdError):
                if _DEVICE_NOT_CONNECTED in str(err):
                    raise errors.DeviceNotConnectedError(
                        f"{self._target_name} is not connected to host"
                    ) from err
                raise ffx_errors.FfxCommandError(err) from err
            else:
                # TODO(b/520414073): It's worth treating the ffx internal
                # command error i.e. "BUG: An internal command error occurred."
                # differently and retry the command.
                raise ffx_errors.FfxTimeoutError(err) from err

    def popen(  # type: ignore[no-untyped-def]
        self,
        cmd: list[str],
        config_overrides: dict[str, Any] | None = None,
        **kwargs,
    ) -> subprocess.Popen[custom_types.AnyString]:
        """Starts a new process to run the FFX cmd and returns the corresponding
        process.

        Intended for executing daemons or processing streamed output. Given
        the raw nature of this API, it is up to callers to detect and handle
        potential errors, and make sure to close this process eventually
        (e.g. with `popen.terminate` method). Otherwise, use the simpler `run`
        method instead.

        Args:
            cmd: FFX command to run.
            config_overrides: FFX configuration overrides.
            kwargs: Forwarded as-is to subprocess.Popen.

        Returns:
            The Popen object of `ffx -t {target} {cmd}`.
            If text arg of subprocess.Popen is set to True,
            subprocess.Popen[str] will be returned. Otherwise,
            subprocess.Popen[bytes] will be returned.
        """
        return host_shell.popen(
            cmd=self.generate_ffx_cmd(
                cmd=cmd,
                machine=MachineFormat.RAW,
                config_overrides=config_overrides,
            ),
            **kwargs,
        )

    def run_test_component(
        self,
        component_url: str,
        ffx_test_args: list[str] | None = None,
        test_component_args: list[str] | None = None,
        capture_output: bool = True,
    ) -> str:
        """Executes and returns the output of
        `ffx -t {target} test run {component_url}` with the given options.

        This results in an invocation:
        ```
        ffx -t {target} test {component_url} {ffx_test_args} -- {test_component_args}`.
        ```

        For example:

        ```
        ffx -t fuchsia-emulator test \
            fuchsia-pkg://fuchsia.com/my_benchmark#test.cm \
            --output_directory /tmp \
            -- /custom_artifacts/results.fuchsiaperf.json
        ```

        Args:
            component_url: The URL of the test to run.
            ffx_test_args: args to pass to `ffx test run`.
            test_component_args: args to pass to the test component.
            capture_output: When True, the stdout/err from the command will be captured and
                returned. When False, the output of the command will be streamed to stdout/err
                accordingly and it won't be returned. Defaults to True.

        Returns:
            Output of `ffx -t {target} {cmd}` when capture_output is set to True, otherwise an
            empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        cmd: list[str] = _FFX_CMDS["TEST_RUN"][:]
        cmd.append(component_url)
        if ffx_test_args:
            cmd += ffx_test_args
        if test_component_args:
            cmd.append("--")
            cmd += test_component_args
        return self.run(cmd, capture_output=capture_output)

    def run_ssh_cmd(
        self,
        cmd: str,
        capture_output: bool = True,
    ) -> str:
        """Executes and returns the output of `ffx -t target ssh <cmd>`.

        Args:
            cmd: SSH command to run.
            capture_output: When True, the stdout/err from the command will be
                captured and returned. When False, the output of the command
                will be streamed to stdout/err accordingly and it won't be
                returned. Defaults to True.

        Returns:
            Output of `ffx -t target ssh <cmd>` when capture_output is set to
            True, otherwise an empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        ffx_cmd: list[str] = _FFX_CMDS["TARGET_SSH"][:]
        ffx_cmd.append(cmd)
        return self.run(
            ffx_cmd, capture_output=capture_output, machine=MachineFormat.RAW
        )

    def wait_for_rcs_connection(
        self,
        include_target_name: bool = False,
        timeout: float | None = None,
    ) -> None:
        """Wait until FFX is able to establish a RCS connection to the target.

        Args:
            include_target_name: If set to True, target will be specified by query.
                Otherwise, target will be specified by address.
            timeout: Maximum time in seconds to wait for the RCS connection.
                If None (default), waits indefinitely.
        Raises:
            DeviceNotConnectedError: If FFX fails to reach target within the
                timeout.
            FfxCommandError: In case of other FFX command failure.
        """
        _LOGGER.info("Waiting for %s to connect to host...", self._target_name)
        deadline: float | None = (
            time.monotonic() + timeout if timeout is not None else None
        )

        def _timed_out() -> bool:
            return deadline is not None and time.monotonic() >= deadline

        if include_target_name:
            # In strict mode, we cannot use nodename with `-t` for `target wait`.
            # Instead, poll `target list` until RCS is connected.

            _LOGGER.info(
                "Waiting for RCS connection via target list polling..."
            )
            while True:
                try:
                    # Run target list without `-t` but with nodename filter as positional arg
                    cmd = ["target", "list", self._target_name]
                    output = self.run(
                        cmd=cmd,
                        include_target=False,
                        log_status_on_failure=False,
                    )
                    targets = json.loads(output)
                    if targets and targets[0].get("rcs_state") == "Y":
                        _LOGGER.info("RCS connected!")
                        break
                except (
                    errors.DeviceNotConnectedError,
                    ffx_errors.FfxCommandError,
                    ffx_errors.FfxTimeoutError,
                    json.JSONDecodeError,
                ) as e:
                    _LOGGER.debug("Error polling target list: %s", e)
                if _timed_out():
                    raise errors.DeviceNotConnectedError(
                        f"{self._target_name} did not connect to host within "
                        f"{timeout} seconds."
                    )
                time.sleep(2)
        else:
            # Let ffx enforce the timeout itself, and bound the host-side
            # process slightly beyond it in case ffx stalls. A value of "0"
            # tells ffx target wait not to time out.
            cmd = _FFX_CMDS["TARGET_WAIT"] + [
                "0" if timeout is None else str(math.ceil(timeout))
            ]
            host_timeout: float | None = (
                None
                if timeout is None
                else timeout + _TARGET_WAIT_HOST_SLACK_SEC
            )
            self.run(
                cmd=cmd,
                timeout=host_timeout,
                disable_controlmaster=True,
            )

        _LOGGER.info("%s is connected to host", self._target_name)
        return

    def wait_for_rcs_disconnection(self) -> None:
        """Wait until FFX is able to disconnect RCS connection to the target.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        _LOGGER.info(
            "Waiting for %s to disconnect from host...",
            self._target_name,
        )
        self.run(
            cmd=_FFX_CMDS["TARGET_WAIT_DOWN"],
            disable_controlmaster=True,
            config_overrides={
                "ssh.connect_timeout": 2,
                "ssh.connection_attempts": 1,
            },
        )
        _LOGGER.info("%s is not connected to host", self._target_name)
        return

    def generate_ffx_cmd(
        self,
        cmd: list[str],
        include_target: bool = True,
        machine: MachineFormat = MachineFormat.JSON,
        disable_controlmaster: bool = False,
        config_overrides: dict[str, Any] | None = None,
    ) -> list[str]:
        """Generates the FFX command that need to be run.

        Args:
            cmd: FFX command.
            include_target: True to include "-t <target>", False otherwise.
            machine: Specifies the machine format used for the ffx command (defaults
                to "json")
            disable_controlmaster: boolean value of controlmaster.
            config_overrides: FFX configuration overrides.

        Returns:
            FFX command to be run as list of string.
        """
        ffx_args: list[str] = []

        # Specifying strict mode and isolate dir are mutually exclusive in ffx.
        # Since we use strict mode, we do not pass isolate_dir.
        ffx_args.extend(["--strict"])

        if include_target:
            target = self.get_target_address()
            ffx_args.extend(["-t", str(target)])

        # Don't add "--machine" if the machine type is already specified
        if "--machine" not in cmd:
            ffx_args.extend(["--machine", str(machine)])

        if disable_controlmaster:
            ffx_args.extend(["-c", "ssh.controlmaster.mode=none"])

        if config_overrides:
            for k, v in config_overrides.items():
                ffx_args.extend(["-c", f"{k}={v}"])

        # Add log file path
        ffx_args.extend(["-o", str(Path(self.config.logs_dir) / "ffx.log")])

        # To run FFX in direct mode
        ffx_args.extend(["--direct"])

        # Inject configuration via command line arguments
        ffx_args.extend(self.config.get_config_args())

        return [self.config.binary_path] + ffx_args + cmd

    def _on_device_address_change(
        self, target_addr: custom_types.TargetAddr
    ) -> None:
        """Callback method that gets invoked when device address changes.

        Args:
            target_addr: New target address of the device.
        """
        self._target_addr = target_addr

        self.check_connection()
