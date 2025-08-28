#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion.controllers.fuchsia_lib.base_lib import BaseLib


class FuchsiaWlanDeprecatedConfigurationLib(BaseLib):
    def __init__(self, addr: str) -> None:
        super().__init__(addr, "wlan_deprecated")

    def wlanSuggestAccessPointMacAddress(self, addr: str) -> dict[str, str]:
        """Suggests a mac address to soft AP interface, to support
        cast legacy behavior.

        Args:
            addr: string of mac address to suggest (e.g. '12:34:56:78:9a:bc')
        """
        test_cmd = "wlan_deprecated.suggest_ap_mac"
        test_args = {"mac": addr}

        return self.send_command(test_cmd, test_args)
