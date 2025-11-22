# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FFX transport ABC implementation"""

import ipaddress
import json
import logging
import subprocess
from dataclasses import asdict
from pathlib import Path
from typing import Any

from honeydew import errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_interface
from honeydew.transports.ffx.types import MonitorTargetInfo, TargetInfoData
from honeydew.typing import custom_types
from honeydew.utils import host_shell, properties

_FFX_BINARY: str = "ffx"

_FFX_CMDS: dict[str, list[str]] = {
    "TARGET_ADD": ["target", "add"],
    "TARGET_SHOW": ["--machine", "json", "target", "show"],
    "TARGET_SSH_ADDRESS": ["target", "list", "--format", "addresses"],
    "TARGET_LIST": ["--machine", "json", "target", "list"],
    "TARGET_WAIT": ["target", "wait", "--timeout", "0"],
    "TARGET_WAIT_DOWN": ["target", "wait", "--down", "--timeout", "0"],
    # Tell the daemon to drop its connection to the target
    "TARGET_DISCONNECT": ["daemon", "disconnect"],
    "TEST_RUN": ["test", "run"],
    "TARGET_SSH": ["target", "ssh"],
    "MONITOR_STATUS": ["--machine", "json", "monitor", "status"],
    "MONITOR_CONFIG_GET": ["config", "get", "monitor.pid_file"],
}

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEVICE_NOT_CONNECTED: str = "Timeout attempting to reach target"


class FfxImpl(ffx_interface.FFX):
    """Provides methods for Host-(Fuchsia)Target interactions via FFX.

    Args:
        target_name: Fuchsia device name.
        config: Configuration associated with FFX and FFX daemon.
        target_ip_port: Fuchsia device IP address and port.

        Note: When target_ip is provided, it will be used instead of target_name
        while running ffx commands (ex: `ffx -t <target_ip> <command>`).
        use_monitor_state: True to use ffx monitor for target status, False
            otherwise.

    Raises:
        FfxConnectionError: In case of failed to check FFX connection.
        FfxCommandError: In case of failure.
    """

    def __init__(
        self,
        target_name: str,
        config_data: ffx_config.FfxConfigData,
        target_ip_port: custom_types.IpPort | None = None,
        use_monitor_state: bool = False,
        shared_data: str | None = None,
    ) -> None:
        invalid_target_name: bool = False
        try:
            ipaddress.ip_address(target_name)
            invalid_target_name = True
        except ValueError:
            pass
        if invalid_target_name:
            raise ValueError(
                f"{target_name=} is an IP address instead of target name"
            )

        self._config_data: ffx_config.FfxConfigData = config_data

        self._target_name: str = target_name

        self._target_ip_port: custom_types.IpPort | None = target_ip_port

        self._target: str
        if self._target_ip_port:
            self._target = str(self._target_ip_port)
        else:
            self._target = self._target_name

        self._shared_data = shared_data
        self._use_monitor = use_monitor_state
        if use_monitor_state and not self._check_running_monitor():
            raise ffx_errors.FFXMonitorNotSupportedError(
                "No running monitor detected."
            )
        _LOGGER.info("Use FFX Monitor Session: %s", self._use_monitor)

        if self._target_ip_port:
            self.add_target()
        self.check_connection()

    @properties.PersistentProperty
    def config(self) -> ffx_config.FfxConfigData:
        """Returns the FFX configuration associated with this instance of FFX
        object.

        Returns:
            FFXConfig
        """
        return self._config_data

    def add_target(self) -> None:
        """Adds a target to the ffx collection

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        cmd: list[str] = _FFX_CMDS["TARGET_ADD"] + [str(self._target_ip_port)]
        ffx_cmd: list[str] = self.generate_ffx_cmd(
            cmd=cmd, include_target=False
        )

        try:
            host_shell.run(cmd=ffx_cmd)
        except errors.HostCmdError as err:
            if _DEVICE_NOT_CONNECTED in str(err):
                raise errors.DeviceNotConnectedError(
                    f"{self._target_name} is not connected to host"
                ) from err
            raise ffx_errors.FfxCommandError(err) from err

    # FFX monitor session management:
    # For infra runs, ffx monitor session is started and managed by botanist.
    # Currently it is not started by default and is only started for on a
    # specific builder basis. But once ffx monitor is thoroughly tested, it
    # will be started by default on all infra builders.
    #
    # For local runs, ffx monitor eventually will be started and managed by
    # `fx test`. Until this support is added (b/455924189), local users would
    # have to manually start monitors through `ffx monitor start`.
    def _check_running_monitor(self) -> bool:
        """Check whether there is a running monitor.

        Returns:
            True if a monitor is running.
        """
        cmd: list[str] = []
        if self._shared_data:
            cmd.extend(["-c", f"shared_data={self._shared_data}"])
        cmd.extend(_FFX_CMDS["MONITOR_CONFIG_GET"])
        output: str = self.run(cmd=cmd).strip('"')
        _LOGGER.info(
            "Fetched config: `%s` returned: %s path exists: `%s`",
            " ".join(cmd),
            output,
            Path(output).exists(),
        )

        # If the pid_path exist, it means there is a running monitor.
        return Path(output).exists()

    def check_connection(self) -> None:
        """Checks the FFX connection from host to Fuchsia device.

        Raises:
            FfxConnectionError
        """
        try:
            self.wait_for_rcs_connection()
        except errors.HoneydewError as err:
            raise ffx_errors.FfxConnectionError(
                f"FFX connection check failed for {self._target_name} with err: {err}"
            ) from err

    def get_target_information(self) -> TargetInfoData:
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

    # TODO(b/455928356) This method would be removed in favor of `get_target_status`.
    def get_target_info_from_target_list(self) -> dict[str, Any]:
        """Executed and returns the output of
        `ffx --machine json target list <target>`.

        For monitor, the "targets" field of `ffx monitor status`
        response is identical with `ffx target list`, so they can share
        the same parsing logic.

        Returns:
            Output of `ffx --machine json target list <target>`.

        Raises:
            FfxCommandError: In case of FFX command failure.
        """
        if self._use_monitor:
            return asdict(self._get_target_status())

        cmd: list[str] = _FFX_CMDS["TARGET_LIST"] + [self._target]
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
                f"'{self._target_name}' is not connected to host"
            )

    def get_target_name(self) -> str:
        """Returns the target name.

        Returns:
            Target name.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        if self._use_monitor:
            target = self._get_target_status()
            return target.nodename

        ffx_target_show_info: TargetInfoData = self.get_target_information()
        return ffx_target_show_info.target.name

    def get_target_ssh_address(self) -> custom_types.TargetSshAddress:
        """Returns the target's ssh ip address and port information.

        Returns:
            (Target SSH IP Address, Target SSH Port)

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        if self._use_monitor:
            target = self._get_target_status()
            address = target.addresses[0]

            return custom_types.TargetSshAddress(
                ip=address.ip,
                port=address.port,
            )

        cmd: list[str] = _FFX_CMDS["TARGET_SSH_ADDRESS"]
        output: str = self.run(cmd=cmd)

        # in '[fe80::6a47:a931:1e84:5077%qemu]:22', ":22" is SSH port.
        # Ports can be 1-5 chars, clip off everything after the last ':'.
        ssh_info: list[str] = output.rsplit(":", 1)
        ssh_ip: str = ssh_info[0].replace("[", "").replace("]", "")
        ssh_port: int = int(ssh_info[1])

        return custom_types.TargetSshAddress(
            ip=ipaddress.ip_address(ssh_ip), port=ssh_port
        )

    def get_target_board(self) -> str:
        """Returns the target's board.

        Returns:
            Target's board.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        if self._use_monitor:
            target = self._get_target_status()
            return target.target_type.split(".")[1]

        target_show_info: TargetInfoData = self.get_target_information()
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
        if self._use_monitor:
            target = self._get_target_status()
            return target.target_type.split(".")[0]

        target_show_info: TargetInfoData = self.get_target_information()
        return (
            target_show_info.build.product
            if target_show_info.build.product
            else ""
        )

    def run(
        self,
        cmd: list[str],
        timeout: float | None = None,
        capture_output: bool = True,
        log_output: bool = True,
        include_target: bool = True,
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
        )
        try:
            return (
                host_shell.run(
                    cmd=ffx_cmd,
                    capture_output=capture_output,
                    log_output=log_output,
                    timeout=timeout,
                )
                or ""
            )
        except errors.HostCmdError as err:
            if _DEVICE_NOT_CONNECTED in str(err):
                raise errors.DeviceNotConnectedError(
                    f"{self._target_name} is not connected to host"
                ) from err
            raise ffx_errors.FfxCommandError(err) from err
        except errors.HoneydewTimeoutError as err:
            raise ffx_errors.FfxTimeoutError(err) from err

    def popen(  # type: ignore[no-untyped-def]
        self,
        cmd: list[str],
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
            kwargs: Forwarded as-is to subprocess.Popen.

        Returns:
            The Popen object of `ffx -t {target} {cmd}`.
            If text arg of subprocess.Popen is set to True,
            subprocess.Popen[str] will be returned. Otherwise,
            subprocess.Popen[bytes] will be returned.
        """
        return host_shell.popen(
            cmd=self.generate_ffx_cmd(cmd=cmd),
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
        ffx -t fuchsia-emulator test \\
            fuchsia-pkg://fuchsia.com/my_benchmark#test.cm \\
            --output_directory /tmp \\
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
        return self.run(ffx_cmd, capture_output=capture_output)

    def wait_for_rcs_connection(self) -> None:
        """Wait until FFX is able to establish a RCS connection to the target.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        _LOGGER.info("Waiting for %s to connect to host...", self._target_name)
        if self._use_monitor:
            while True:
                target = self._get_target_status()
                if target.target_state == "Product" and target.rcs_state == "Y":
                    _LOGGER.info("%s is connected to host", self._target_name)
                    return

        self.run(cmd=_FFX_CMDS["TARGET_WAIT"])

        _LOGGER.info("%s is connected to host", self._target_name)
        return

    def wait_for_rcs_disconnection(self) -> None:
        """Wait until FFX is able to disconnect RCS connection to the target.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        _LOGGER.info(
            "Waiting for %s to disconnect from host...", self._target_name
        )
        self.run(cmd=_FFX_CMDS["TARGET_WAIT_DOWN"])
        _LOGGER.info("%s is not connected to host", self._target_name)

        _LOGGER.debug(
            "Informing the FFX daemon to drop the connection to %s",
            self._target_name,
        )
        self.run(cmd=_FFX_CMDS["TARGET_DISCONNECT"])

        return

    def _get_target_status(self) -> MonitorTargetInfo:
        """Gets the status information of the target node from 'ffx monitor status'.

        This method is valid only when 'ffx monitor start` session was running
        for this target. It parses the output of `ffx --machine json monitor status`
        to find the dictionary corresponding to the current target node.

        Args:
            None

        Returns:
            A MonitorTargetInfo containing the status information of the target
            node, an empty MonitorTargetInfo if the target is not found in the
            monitor output.
        Raises:
            ffx_errors.FFXMonitorNotSupportedError: If this method is called when monitor is not in use.
        """
        if not self._use_monitor:
            raise ffx_errors.FFXMonitorNotSupportedError(
                "_get_target_status can only be called when ffx monitor is in"
                " use."
            )
        cmd = []
        if self._shared_data:
            cmd.extend(["-c", f"shared_data={self._shared_data}"])
        cmd.extend(_FFX_CMDS["MONITOR_STATUS"])
        statuses = self.run(
            cmd=cmd,
            include_target=False,
        )
        _LOGGER.info("DEBUG: statuses: %s", statuses)
        targets = json.loads(statuses).get("targets", [])
        for target in targets:
            if target["nodename"] == self._target_name:
                addresses = []
                for addr in target.get("addresses", []):
                    ssh_port = addr["ssh_port"]
                    if ssh_port == 0:
                        ssh_port = None

                    addresses.append(
                        custom_types.IpPort(ip=addr["ip"], port=ssh_port)
                    )
                target["addresses"] = addresses
                return MonitorTargetInfo(
                    **target
                )  # pytype: disable=wrong-arg-types
        return MonitorTargetInfo()

    def generate_ffx_cmd(
        self,
        cmd: list[str],
        include_target: bool = True,
    ) -> list[str]:
        """Generates the FFX command that need to be run.

        Args:
            cmd: FFX command.
            include_target: True to include "-t <target_name>", False otherwise.

        Returns:
            FFX command to be run as list of string.
        """
        ffx_args: list[str] = []

        if include_target:
            ffx_args.extend(["-t", f"{self._target}"])

        # To run FFX in isolation mode
        ffx_args.extend(["--isolate-dir", self.config.isolate_dir.directory()])

        return [self.config.binary_path] + ffx_args + cmd
