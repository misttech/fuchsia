#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import ipaddress
import logging

from antlion.controllers.fuchsia_lib.ssh import FuchsiaSSHProvider
from antlion.controllers.fuchsia_lib.wlan_deprecated_configuration_lib import (
    FuchsiaWlanDeprecatedConfigurationLib,
)
from antlion.net import wait_for_port
from antlion.runner import CalledProcessError
from mobly import logger

DEFAULT_SL4F_PORT = 80
START_SL4F_V2_CMD = "start_sl4f"


class SL4F:
    """Module for Fuchsia devices to interact with the SL4F tool.

    Attributes:
        ssh: Transport to start and stop SL4F.
        address: http address for SL4F server including SL4F port.
        log: Logger for the device-specific instance of SL4F.
    """

    def __init__(
        self,
        ssh: FuchsiaSSHProvider,
        port: int = DEFAULT_SL4F_PORT,
    ) -> None:
        """
        Args:
            ssh: Transport to start and stop SL4F.
            port: Port for the SL4F server to listen on.
        """
        ip = ipaddress.ip_address(ssh.config.host_name)
        if ip.version == 4:
            self.address = f"http://{ip}:{port}"
        elif ip.version == 6:
            self.address = f"http://[{ip}]:{port}"

        self.log = logger.PrefixLoggerAdapter(
            logging.getLogger(),
            {
                logger.PrefixLoggerAdapter.EXTRA_KEY_LOG_PREFIX: f"[SL4F | {self.address}]",
            },
        )

        try:
            ssh.stop_component("sl4f")
            ssh.run(START_SL4F_V2_CMD).stdout
        except CalledProcessError:
            # TODO(fxbug.dev/42181764) Remove support to run SL4F in CFv1 mode
            # once ACTS no longer use images that comes with only CFv1 SL4F.
            self.log.warn(
                "Running SL4F in CFv1 mode, "
                "this is deprecated for images built after 5/9/2022, "
                "see https://fxbug.dev/42157029 for more info."
            )
            ssh.stop_component("sl4f")
            ssh.start_v1_component("sl4f")

        try:
            wait_for_port(ssh.config.host_name, port)
            self.log.info("SL4F server is reachable")
        except TimeoutError as e:
            raise TimeoutError("SL4F server is unreachable") from e

        self._init_libraries()

    def _init_libraries(self) -> None:
        # Grabs command from FuchsiaWlanDeprecatedConfigurationLib
        self.wlan_deprecated_configuration_lib = (
            FuchsiaWlanDeprecatedConfigurationLib(self.address)
        )
