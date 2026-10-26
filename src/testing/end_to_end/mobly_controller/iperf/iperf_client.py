#!/usr/bin/env python3
#
# Copyright 2026 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Blanket ignores to enable mypy in Antlion
# mypy: disable-error-code="no-untyped-def, attr-defined"

import logging
import os
import subprocess
import threading
from abc import ABC, abstractmethod

from libs.commands.date import LinuxDateCommand
from libs.ssh.provider import SSHConfig, SSHProvider
from libs.types import ControllerConfig, Json
from libs.validation import MapValidator

MOBLY_CONTROLLER_CONFIG_NAME: str = "IPerfClient"


def create(configs: list[ControllerConfig]) -> list[IPerfClientBase]:
    """Factory method for iperf clients.

    The function creates iperf clients based on config.
    If configs contain ssh settings, remote iperf clients
    over ssh will be started on those devices.

    Args:
        configs: config parameters for the iperf client

    Returns:
        A list of iperf client objects connected over SSH.

    Raises:
        ValueError: If a config entry is missing 'ssh_config'.
    """
    results: list[IPerfClientBase] = []
    for config in configs:
        c = MapValidator(config)
        if "ssh_config" in config:
            results.append(
                IPerfClientOverSsh(
                    SSHProvider(
                        SSHConfig.from_config(c.get(dict, "ssh_config"))
                    ),
                    test_interface=c.get(str, "test_interface"),
                    sync_date=c.get(bool, "sync_date", True),
                )
            )
        else:
            raise ValueError(
                f"Config entry {config} in {configs} is missing 'ssh_config'."
            )
    return results


def destroy(objects: list[IPerfClientBase]) -> None:
    # No cleanup needed.
    pass


def get_info(objects: list[IPerfClientBase]) -> list[Json]:
    return []


class IPerfClientBase(ABC):
    """The Base class for all IPerfClients.

    This base class is responsible for synchronizing the logging to prevent
    multiple IPerfClients from writing results to the same file, as well
    as providing the interface for IPerfClient objects.
    """

    # Keeps track of the number of IPerfClient logs to prevent file name
    # collisions.
    __log_file_counter = 0

    __log_file_lock = threading.Lock()

    @property
    @abstractmethod
    def test_interface(self) -> str | None:
        """Find the test interface.

        Returns:
            Name of the interface used to communicate with server_ap, or None if
            not set.
        """
        ...

    @staticmethod
    def _get_full_file_path(tag: str = "") -> str:
        """Returns the full file path for the IPerfClient log file.

        Note: If the directory for the file path does not exist, it will be
        created.

        Args:
            tag: The tag passed in to the server run.
        """
        # Set by Mobly in base_test.BaseTestClass.run.
        full_out_dir = os.path.join(
            getattr(logging, "log_path"), "iperf_client_files"
        )

        with IPerfClientBase.__log_file_lock:
            os.makedirs(full_out_dir, exist_ok=True)
            tags = ["IPerfClient", tag, IPerfClientBase.__log_file_counter]
            out_file_name = "%s.log" % (
                ",".join([str(x) for x in tags if x != "" and x is not None])
            )
            IPerfClientBase.__log_file_counter += 1

        return os.path.join(full_out_dir, out_file_name)

    def start(
        self,
        ip: str,
        iperf_args: str,
        tag: str,
        timeout: int = 3600,
        iperf_binary: str | None = None,
    ) -> str:
        """Starts iperf client, and waits for completion.

        Args:
            ip: iperf server ip address.
            iperf_args: A string representing arguments to start iperf
                client. Eg: iperf_args = "-t 10 -p 5001 -w 512k/-u -b 200M -J".
            tag: A string to further identify iperf results file
            timeout: the maximum amount of time the iperf client can run.
            iperf_binary: Location of iperf3 binary. If none, it is assumed
                the binary is in the path.

        Returns:
            full_out_path: iperf result path.
        """
        raise NotImplementedError("start() must be implemented.")


class IPerfClientOverSsh(IPerfClientBase):
    """Class that handles iperf3 client operations on remote machines."""

    def __init__(
        self,
        ssh_provider: SSHProvider,
        test_interface: str | None = None,
        sync_date: bool = True,
    ):
        self._ssh_provider = ssh_provider
        self._test_interface = test_interface

        if sync_date:
            # iperf clients are not given internet access, so their system time
            # needs to be manually set to be accurate.
            LinuxDateCommand(self._ssh_provider).sync()

    @property
    def test_interface(self) -> str | None:
        return self._test_interface

    def start(
        self,
        ip: str,
        iperf_args: str,
        tag: str,
        timeout: int = 3600,
        iperf_binary: str | None = None,
    ) -> str:
        """Starts iperf client, and waits for completion.

        Args:
            ip: iperf server ip address.
            iperf_args: A string representing arguments to start iperf
            client. Eg: iperf_args = "-t 10 -p 5001 -w 512k/-u -b 200M -J".
            tag: tag to further identify iperf results file
            timeout: the maximum amount of time to allow the iperf client to run
            iperf_binary: Location of iperf3 binary. If none, it is assumed
                the binary is in the path.

        Returns:
            full_out_path: iperf result path.
        """
        if not iperf_binary:
            logging.debug(
                "No iperf3 binary specified.  "
                "Assuming iperf3 is in the path."
            )
            iperf_binary = "iperf3"
        else:
            logging.debug(f"Using iperf3 binary located at {iperf_binary}")
        iperf_cmd = f"{iperf_binary} -c {ip} {iperf_args}"
        full_out_path = self._get_full_file_path(tag)

        try:
            iperf_process = self._ssh_provider.run(
                iperf_cmd, timeout_sec=timeout
            )
            stdout = iperf_process.stdout or b""
        except subprocess.CalledProcessError as err:
            # Capture output on non-zero exit (e.g. server busy) so error details
            # are preserved in the log file instead of lost.
            logging.warning(f"iperf client exited with error: {err}")
            stdout = err.stdout or b""
        except subprocess.TimeoutExpired as err:
            raise TimeoutError(
                f"Command execution timed out waiting for remote iperf client after {timeout} seconds: {err}"
            ) from err

        with open(full_out_path, "wb") as out_file:
            out_file.write(stdout)

        return full_out_path
