# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Provides methods to configure FFX."""

import atexit
import logging
from dataclasses import dataclass

import fuchsia_controller_py as fuchsia_controller

from honeydew import errors

_FFX_BINARY: str = "ffx"

_LOGGER: logging.Logger = logging.getLogger(__name__)


class FfxConfigError(errors.HoneydewError):
    """Raised by FfxConfig class."""


@dataclass(frozen=True)
class FfxConfigData:
    """Dataclass that holds FFX config information.

    Args:
        binary_path: absolute path to the FFX binary.
        isolate_dir: Directory that will be passed to `--isolate-dir`
            arg of FFX
        logs_dir: Directory that will be passed to `--config log.dir`
            arg of FFX
        logs_level: logs level that will be passed to `--config log.level`
            arg of FFX
        enable_mdns: Whether or not mdns need to be enabled. This will be
            passed to `--config discovery.mdns.enabled` arg of FFX
        subtools_search_path: A path of where ffx should
            look for plugins.
        proxy_timeout_secs: Proxy timeout in secs.
        ssh_keepalive_timeout: SSH keep-alive timeout in secs.
    """

    binary_path: str
    isolate_dir: fuchsia_controller.IsolateDir
    logs_dir: str
    logs_level: str | None
    mdns_enabled: bool
    subtools_search_path: str | None
    proxy_timeout_secs: int | None
    ssh_keepalive_timeout: int | None

    def __str__(self) -> str:
        return (
            f"binary_path={self.binary_path}, "
            f"isolate_dir={self.isolate_dir.directory()}, "
            f"logs_dir={self.logs_dir}, "
            f"logs_level={self.logs_level}, "
            f"mdns_enabled={self.mdns_enabled}, "
            f"subtools_search_path={self.subtools_search_path}, "
            f"proxy_timeout_secs={self.proxy_timeout_secs}, "
            f"ssh_keepalive_timeout={self.ssh_keepalive_timeout}, "
        )

    def get_config_args(self) -> list[str]:
        """Returns the FFX command arguments to set the configuration.

        Returns:
            List of FFX command arguments.
        """
        configs = {
            "log.dir": self.logs_dir,
            "log.level": self.logs_level,
            "discovery.mdns.enabled": str(self.mdns_enabled).lower(),
            "ffx.subtool-search-paths": self.subtools_search_path,
            "proxy.timeout_secs": self.proxy_timeout_secs,
            "ssh.keepalive_timeout": self.ssh_keepalive_timeout,
        }

        ffx_args = []
        for key, value in configs.items():
            if value is not None:
                ffx_args.extend(["-c", f"{key}={value}"])
        return ffx_args


class FfxConfig:
    """Provides methods to configure FFX."""

    def __init__(self) -> None:
        self._setup_done: bool = False

    def setup(
        self,
        binary_path: str | None,
        isolate_dir: str | None,
        logs_dir: str,
        logs_level: str | None,
        enable_mdns: bool,
        subtools_search_path: str | None = None,
        proxy_timeout_secs: int | None = None,
        ssh_keepalive_timeout: int | None = None,
    ) -> None:
        """Sets up configuration need to be used while running FFX command.

        Args:
            binary_path: absolute path to the FFX binary.
            isolate_dir: Directory that will be passed to `--isolate-dir`
                arg of FFX. If set to None, a random directory will be created.
            logs_dir: Directory that will be passed to `--config log.dir`
                arg of FFX
            logs_level: logs level that will be passed to `--config log.level`
                arg of FFX
            enable_mdns: Whether or not mdns need to be enabled. This will be
                passed to `--config discovery.mdns.enabled` arg of FFX
            subtools_search_path: A path of where ffx should look for plugins.
                Default value is None which means, it will not update
                proxy_timeout_secs
            proxy_timeout_secs: Proxy timeout in secs. Default value is None
                which means, it will not update proxy_timeout_secs
            ssh_keepalive_timeout: SSH keep-alive timeout in secs.
                Default value is None which means, it will not update
                ssh_keepalive_timeout

        Raises:
            FfxConfigError: If setup has already been called once.
        """
        if self._setup_done:
            raise FfxConfigError("setup has already been called once.")

        # Ensure clean up occurs upon normal program termination.
        atexit.register(self._atexit_callback)

        self._ffx_binary: str = binary_path if binary_path else _FFX_BINARY
        self._isolate_dir: fuchsia_controller.IsolateDir | None = (
            fuchsia_controller.IsolateDir(isolate_dir)
        )
        self._logs_dir: str = logs_dir
        self._logs_level: str | None = logs_level
        self._mdns_enabled: bool = enable_mdns
        self._subtools_search_path: str | None = subtools_search_path
        self._proxy_timeout_secs: int | None = proxy_timeout_secs
        self._ssh_keepalive_timeout: int | None = ssh_keepalive_timeout

        self._setup_done = True

    def close(self) -> None:
        """Clean up method.

        Raises:
            FfxConfigError: When called before calling `FfxConfig.setup`
        """
        if self._setup_done is False:
            raise FfxConfigError("close called before calling setup.")

        # Setting to None will delete the `self._isolate_dir.directory()`
        self._isolate_dir = None

        self._setup_done = False

    def get_config(self) -> FfxConfigData:
        """Returns the FFX configuration information that has been set.

        Returns:
            FfxConfigData

        Raises:
            FfxConfigError: When called before `FfxConfig.setup` or after `FfxConfig.close`.
        """
        if self._setup_done is False:
            raise FfxConfigError("get_config called before calling setup.")
        if self._isolate_dir is None:
            raise FfxConfigError("get_config called after calling close.")

        return FfxConfigData(
            binary_path=self._ffx_binary,
            isolate_dir=self._isolate_dir,
            logs_dir=self._logs_dir,
            logs_level=self._logs_level,
            mdns_enabled=self._mdns_enabled,
            subtools_search_path=self._subtools_search_path,
            proxy_timeout_secs=self._proxy_timeout_secs,
            ssh_keepalive_timeout=self._ssh_keepalive_timeout,
        )

    def _atexit_callback(self) -> None:
        try:
            self.close()
        except FfxConfigError:
            pass
