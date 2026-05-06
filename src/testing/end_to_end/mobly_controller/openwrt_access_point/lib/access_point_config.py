# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data classes and enums for Wi-Fi Configuration."""
import dataclasses
import enum
import random
import re
import string
from typing import Literal, Optional, Protocol, TypeAlias


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


Bandwidth: TypeAlias = Literal[20, 40, 80, 160, 320]
WifiModeStr: TypeAlias = Literal["HT", "VHT", "HE", "EHT", "NOHT"]


class PhyMode(Protocol):
    """Protocol to abstract Wi-Fi modes and their associated bandwidth settings."""

    @property
    def mode_str(self) -> WifiModeStr:
        """Returns the base Wi-Fi standard string."""
        ...

    @property
    def bandwidth(self) -> Bandwidth:
        """Returns the channel bandwidth in MHz."""
        ...

    @property
    def uci_htmode(self) -> str:
        """Returns the string used for OpenWrt's wireless 'htmode' setting."""
        ...


@dataclasses.dataclass(frozen=True)
class LegacyMode:
    """802.11a/b/g (No HT) implementation."""

    @property
    def mode_str(self) -> WifiModeStr:
        return "NOHT"

    @property
    def bandwidth(self) -> Bandwidth:
        return 20

    @property
    def uci_htmode(self) -> str:
        return "NOHT"


@dataclasses.dataclass(frozen=True)
class HtMode:
    """802.11n (HT) implementation, handling optional extension channels."""

    bw: Literal[20, 40]
    extension: Optional[Literal["+", "-"]] = None

    def __post_init__(self) -> None:
        if self.bw == 40 and not self.extension:
            raise ValueError("HT40 requires extension channel (+ or -)")

    @property
    def mode_str(self) -> WifiModeStr:
        return "HT"

    @property
    def bandwidth(self) -> Bandwidth:
        return self.bw

    @property
    def uci_htmode(self) -> str:
        ext = self.extension or ""
        return f"HT{self.bw}{ext}"


@dataclasses.dataclass(frozen=True)
class VhtMode:
    """802.11ac (VHT) implementation."""

    bw: Literal[20, 40, 80, 160]

    @property
    def mode_str(self) -> WifiModeStr:
        return "VHT"

    @property
    def bandwidth(self) -> Bandwidth:
        return self.bw

    @property
    def uci_htmode(self) -> str:
        return f"VHT{self.bw}"


@dataclasses.dataclass(frozen=True)
class HeMode:
    """802.11ax (HE) implementation."""

    bw: Literal[20, 40, 80, 160]

    @property
    def mode_str(self) -> WifiModeStr:
        return "HE"

    @property
    def bandwidth(self) -> Bandwidth:
        return self.bw

    @property
    def uci_htmode(self) -> str:
        return f"HE{self.bw}"


@dataclasses.dataclass(frozen=True)
class EhtMode:
    """802.11be (EHT) implementation."""

    bw: Literal[20, 40, 80, 160, 320]

    @property
    def mode_str(self) -> WifiModeStr:
        return "EHT"

    @property
    def bandwidth(self) -> Bandwidth:
        return self.bw

    @property
    def uci_htmode(self) -> str:
        return f"EHT{self.bw}"


@dataclasses.dataclass(frozen=True)
class BssChannel:
    """Represents a Wi-Fi channel configuration using the PhyMode protocol.

    Attributes:
        band: The Wi-Fi frequency band (e.g., 2G, 5G).
        number: The primary channel number.
        phy_mode: An implementation of PhyMode.
    """

    band: Band
    number: int
    phy_mode: PhyMode


DEFAULT_2G_CHANNEL = BssChannel(Band.BAND_2G, 1, HtMode(bw=40, extension="+"))
DEFAULT_5G_CHANNEL = BssChannel(Band.BAND_5G, 36, VhtMode(bw=80))


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


@dataclasses.dataclass(frozen=True)
class CapabilitySelection:
    """Defines the selection of Wi-Fi capabilities."""

    mode: Literal["DEFAULT", "DISABLED", "CUSTOM"]
    capabilities: list[str] = dataclasses.field(default_factory=list)

    @classmethod
    def DEFAULT(cls) -> "CapabilitySelection":
        """Use OpenWrt system defaults."""
        return cls(mode="DEFAULT")

    @classmethod
    def DISABLED(cls) -> "CapabilitySelection":
        """Explicitly disable all capabilities."""
        return cls(mode="DISABLED")

    @classmethod
    def CUSTOM(cls, capabilities: list[str]) -> "CapabilitySelection":
        """Provide a custom list of capabilities to enable."""
        return cls(mode="CUSTOM", capabilities=capabilities)


# TODO(https://fxbug.dev/489258440): Make channel required param and provide easy way to use
# default 2g/5g channels.
@dataclasses.dataclass
class RadioConfig:
    """Configuration required to set up a specific radio on an Access Point.

    Attributes:
        channel: The specific channel within the band
        bss_settings: The settings for all additional bss
        country: The country code for the radio (default: "US")
        n_capabilities: Selection of 802.11n capabilities.
        ac_capabilities: Selection of 802.11ac capabilities.
        require_mode: Forces the radio to operate in a specific 802.11 mode.
            Corresponds to the 'require_mode' option in OpenWrt's wireless
            configuration. Useful for testing scenarios that need to ensure
            the AP is only advertising/allowing clients of a specific standard
            (e.g., 'n' for 802.11n, 'ac' for 802.11ac, 'ax' for 802.11ax).
    """

    channel: BssChannel
    bss_settings: list[BssSettings] | None = None
    country: str = "US"
    n_capabilities: CapabilitySelection = CapabilitySelection.DEFAULT()
    ac_capabilities: CapabilitySelection = CapabilitySelection.DEFAULT()
    require_mode: Literal["n", "ac", "ax", None] = None

    @classmethod
    def generate(
        cls,
        channel: BssChannel,
        bss_settings: list[BssSettings] | None = None,
        country: str = "US",
        n_capabilities: CapabilitySelection = CapabilitySelection.DEFAULT(),
        ac_capabilities: CapabilitySelection = CapabilitySelection.DEFAULT(),
        require_mode: Literal["n", "ac", "ax", None] = None,
    ) -> "RadioConfig":
        """Creates a RadioConfig object with the specified channel and BSS settings.

        Args:
            channel: The Wi-Fi channel configuration.
            bss_settings: Optional list of additional BSS settings.
            country: The country code for the radio.
            n_capabilities: Selection of 802.11n capabilities.
            ac_capabilities: Selection of 802.11ac capabilities.
            require_mode: UCI require_mode value.

        Returns:
            A RadioConfig object.
        """
        if bss_settings is None:
            bss_settings = []

        return cls(
            channel=channel,
            bss_settings=bss_settings,
            country=country,
            n_capabilities=n_capabilities,
            ac_capabilities=ac_capabilities,
            require_mode=require_mode,
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
