# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data classes and enums for Wi-Fi Configuration."""
import dataclasses
import enum
import random
import re
import string
from typing import Literal


class Band(enum.StrEnum):
    """The Wi-Fi frequency band."""

    BAND_2G = "2G"
    BAND_5G = "5G"


# TODO(https://fxbug.dev/487800358): Create to_fidl function.
class Security(enum.StrEnum):
    """The security protocol used for the Wi-Fi network."""

    NONE = "none"
    WPA = "psk"
    WPA2 = "psk2"
    WPA3 = "sae"
    # Mixed modes
    WPA_WPA2 = "psk-mixed"
    WPA2_WPA3 = "sae-mixed"
    WEP = "wep"


@dataclasses.dataclass(frozen=True)
class BssChannel:
    band: Band
    number: int
    bandwidth: Literal[20, 40, 80, 160, 320]


DEFAULT_2G_CHANNEL = BssChannel(Band.BAND_2G, 1, 20)
DEFAULT_5G_CHANNEL = BssChannel(Band.BAND_5G, 36, 40)


@dataclasses.dataclass
class BssSettings:
    """Settings for a BSS (Multiple SSIDs on the same radio).

    Attributes:
        ssid: The Service Set Identifier (network name)
        security: The security encryption protocol
        password: The passphrase or key for the network
    """

    ssid: str
    security: Security
    password: str | None = None
    hidden: bool = False

    @property
    def name(self) -> str:
        """
        Returns a UCI-safe section name based on the SSID.
        Example: "Guest Wi-Fi!" -> "bss_guest_wi_fi"
        """
        # 1. Convert to lowercase
        normalized = self.ssid.lower()
        # 2. Replace non-alphanumeric characters with underscores
        normalized = re.sub(r"[^a-z0-9]+", "_", normalized)

        # 3. Strip leading/trailing underscores and prefix it
        # Prefixing (e.g., 'bss_') prevents issues if an SSID starts with a digit
        safe_name = normalized.strip("_")

        return f"bss_{safe_name}"


# TODO(https://fxbug.dev/489258440): Make channel required param and provide easy way to use
# default 2g/5g channels.
@dataclasses.dataclass
class RadioConfig:
    """Configuration required to set up a specific radio on an Access Point.

    Attributes:
        channel: The specific channel within the band
        bss_settings: The settings for all additional bss
    """

    channel: BssChannel
    bss_settings: list[BssSettings] | None = None

    @classmethod
    def generate(
        cls,
        channel: BssChannel,
        bss_settings: list[BssSettings] | None = None,
    ) -> "RadioConfig":
        """Creates a RadioConfig object with the specified channel and BSS settings.

        Args:
            channel: The Wi-Fi channel configuration.
            bss_settings: Optional list of additional BSS settings.

        Returns:
            A RadioConfig object.
        """
        if bss_settings is None:
            bss_settings = []

        return cls(
            channel=channel,
            bss_settings=bss_settings,
        )


@dataclasses.dataclass
class AccessPointConfig:
    """Configuration required to set up an Access Point.

    Attributes:
        radios: A list of RadioConfig objects defining the radios to configure.
    """

    radios: list[RadioConfig]

    @classmethod
    def random_string(cls, length: int = 8) -> str:
        """Generates a random string of letters and digits.

        Args:
            length: The length of the random string.

        Returns:
            A random string.
        """
        return "".join(
            random.choices(string.ascii_letters + string.digits, k=length)
        )

    @classmethod
    def generate(
        cls,
        radios: list[RadioConfig] | None = None,
    ) -> "AccessPointConfig":
        """Creates an AccessPointConfig containing the specified radio configurations.

        Args:
            radios: A list of RadioConfig objects.

        Returns:
            An AccessPointConfig object.
        """
        if radios is None:
            radios = []

        return cls(radios=radios)
