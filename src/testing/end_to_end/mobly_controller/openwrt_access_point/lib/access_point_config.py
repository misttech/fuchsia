# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data classes and enums for Wi-Fi Configuration."""
import dataclasses
import enum
import random
import string


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


# TODO(https://fxbug.dev/489258440): Make channel required param and provide easy way to use
# default 2g/5g channels.
@dataclasses.dataclass
class AccessPointConfig:
    """Configuration required to set up an Access Point.

    Attributes:
        ssid: The Service Set Identifier (network name)
        password: The passphrase or key for the network
        band: The Wi-Fi frequency band
        channel: The specific channel within the band
        security: The security encryption protocol
        hidden: Whether the network should be hidden
    """

    ssid: str
    band: Band
    channel: int
    security: Security
    password: str | None = None
    hidden: bool = False

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
        security: Security,
        band: Band,
        ssid: str,
        password: str | None = None,
        hidden: bool = False,
    ) -> "AccessPointConfig":
        """Creates an AccessPointConfig, optionally randomizing the SSID and password.

        Args:
            security: The security protocol to use.
            band: The Wi-Fi frequency band.
            ssid: The Service Set Identifier. Randomized if not provided.
            password: The password. Randomized if not provided and security requires it.
            hidden: Whether the network should be hidden. Defaults to False.

        Returns:
            An AccessPointConfig object.
        """
        if password is None and security != Security.NONE:
            raise ValueError(f"Password required for security {security}")
        if password is not None and security == Security.NONE:
            raise ValueError("Password not required for security NONE")

        if band == Band.BAND_2G:
            channel = 1
        elif band == Band.BAND_5G:
            channel = 36

        return cls(
            ssid=ssid,
            password=password,
            band=band,
            channel=channel,
            security=security,
            hidden=hidden,
        )
