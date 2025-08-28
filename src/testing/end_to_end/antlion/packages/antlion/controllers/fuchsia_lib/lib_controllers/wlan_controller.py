#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time

from honeydew.affordances.connectivity.wlan.utils.types import CountryCode
from honeydew.fuchsia_device.fuchsia_device import (
    FuchsiaDevice as HdFuchsiaDevice,
)
from mobly import logger, signals

TIME_TO_SLEEP_BETWEEN_RETRIES = 1
TIME_TO_WAIT_FOR_COUNTRY_CODE = 10


class WlanControllerError(signals.ControllerError):
    pass


class WlanController:
    """Contains methods related to wlan core, to be used in FuchsiaDevice object"""

    def __init__(self, honeydew: HdFuchsiaDevice) -> None:
        self.honeydew = honeydew
        self.log = logger.PrefixLoggerAdapter(
            logging.getLogger(),
            {
                logger.PrefixLoggerAdapter.EXTRA_KEY_LOG_PREFIX: f"[WlanController | {self.honeydew.device_name}]",
            },
        )

    def set_country_code(self, country_code: CountryCode) -> None:
        """Sets country code through the regulatory region service and waits
        for the code to be applied to WLAN PHY.

        Args:
            country_code: the 2 character country code to set

        Raises:
            EnvironmentError - failure to get/set regulatory region
            ConnectionError - failure to query PHYs
        """
        self.log.info(f"Setting DUT country code to {country_code}")
        self.honeydew.wlan_core.set_region(country_code)

        self.log.info(
            f"Verifying DUT country code was correctly set to {country_code}."
        )
        phy_ids_response = self.honeydew.wlan_core.get_phy_id_list()

        end_time = time.time() + TIME_TO_WAIT_FOR_COUNTRY_CODE
        while time.time() < end_time:
            for id in phy_ids_response:
                resp = self.honeydew.wlan_core.get_country(id)
                if resp == country_code:
                    return
                time.sleep(TIME_TO_SLEEP_BETWEEN_RETRIES)
        else:
            raise EnvironmentError(
                f"Failed to set DUT country code to {country_code}."
            )
