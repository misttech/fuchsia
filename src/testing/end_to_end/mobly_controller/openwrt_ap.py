# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
""" Mobly Controller for OpenWRT AP"""

import logging
from typing import Any, Dict, List

from libs.openwrt_lib import OpenwrtAp

MOBLY_CONTROLLER_CONFIG_NAME: str = "OpenWrtAP"


def create(configs: List[Dict[str, Any]]) -> List[OpenwrtAp]:
    """Creates OpenWRT controller objects from testbed configs.

    Args:
      configs: A list of dictionaries, each representing a configuration for
        one OpenWRT device.

    Returns:
      A list of instantiated OpenWRT objects.
    """
    logging.info("Creating OpenWRT controllers with configs: %s", configs)
    return [OpenwrtAp(config) for config in configs]


def destroy(objects: List[OpenwrtAp]) -> None:
    """Destroys OpenWRT controller objects.

    Args:
      objects: A list of OpenWRT objects to be destroyed.
    """
    logging.info("Destroying OpenWRT controllers.")
    for ap in objects:
        ap.close()


def get_info(objects: List[OpenwrtAp]) -> List[str]:
    """Gets information from OpenWRT controller objects.

    Args:
      objects: A list of OpenWRT objects.

    Returns:
      A list of hostnames for each OpenWRT device.
    """
    return [ap.host for ap in objects]
