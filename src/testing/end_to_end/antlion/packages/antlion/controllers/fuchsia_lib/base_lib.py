#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import logging
from typing import Any, Mapping
from urllib.request import Request, urlopen

from mobly.logger import PrefixLoggerAdapter

DEFAULT_SL4F_RESPONSE_TIMEOUT_SEC = 30


class DeviceOffline(Exception):
    """Exception if the device is no longer reachable via the network."""


class SL4FCommandFailed(Exception):
    """A SL4F command to the server failed."""


class BaseLib:
    def __init__(self, addr: str, logger_tag: str) -> None:
        self.address = addr
        self.log = PrefixLoggerAdapter(
            logging.getLogger(),
            {
                PrefixLoggerAdapter.EXTRA_KEY_LOG_PREFIX: f"SL4F | {self.address} | {logger_tag}"
            },
        )

    def send_command(
        self,
        cmd: str,
        args: Mapping[str, object] | None = None,
        response_timeout: float = DEFAULT_SL4F_RESPONSE_TIMEOUT_SEC,
    ) -> dict[str, Any]:
        """Builds and sends a JSON command to SL4F server.

        Args:
            cmd: SL4F method name of command.
            args: Arguments required to execute cmd.
            response_timeout: Seconds to wait for a response before
                throwing an exception.

        Returns:
            Response from SL4F server.

        Throws:
            TimeoutError: The HTTP request timed out waiting for a response
        """
        data = {
            "jsonrpc": "2.0",
            # id is required by the SL4F server to parse test_data but is not
            # currently used.
            "id": "",
            "method": cmd,
            "params": args,
        }
        data_json = json.dumps(data).encode("utf-8")
        req = Request(
            self.address,
            data=data_json,
            headers={
                "Content-Type": "application/json; charset=utf-8",
                "Content-Length": str(len(data_json)),
            },
        )

        self.log.debug(
            f'Sending request "{cmd}" with args: {args} with timeout {response_timeout}'
        )
        response = urlopen(req, timeout=response_timeout)

        response_body = response.read().decode("utf-8")
        try:
            response_json = json.loads(response_body)
            self.log.debug(f'Received response for "{cmd}": {response_json}')
        except json.JSONDecodeError as e:
            raise SL4FCommandFailed(response_body) from e

        # If the SL4F command fails it returns a str, without an 'error' field
        # to get.
        if not isinstance(response_json, dict):
            raise SL4FCommandFailed(response_json)

        return response_json
