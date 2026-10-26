# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data types used by wlan affordance."""

import enum
from dataclasses import dataclass

import fidl_fuchsia_wlan_device_service as f_wlan_device_service
import fidl_fuchsia_wlan_ieee80211 as f_wlan_ieee80211
import fidl_fuchsia_wlan_policy as f_wlan_policy
from honeydew.typing.custom_types import MacAddress as _MacAddress

MacAddress = _MacAddress

# Length of a pre-shared key (PSK) used as a password.
_PSK_LENGTH = 64


def credential_from_password(password: str | None) -> f_wlan_policy.Credential:
    """Parse a password into a fuchsia.wlan.policy/Credential.

    Args:
        password: String password, pre-shared key in hex form with length 64, or
            None/empty to represent open.

    Return:
        A fuchsia.wlan.policy/Credential union object.
    """
    if not password:
        return f_wlan_policy.Credential(none=f_wlan_policy.Empty())
    elif len(password) == _PSK_LENGTH:
        return f_wlan_policy.Credential(psk=list(bytes.fromhex(password)))
    else:
        return f_wlan_policy.Credential(password=list(password.encode("utf-8")))


@dataclass(frozen=True)
class WlanInterfaces:
    """WLAN interfaces separated by device type and keyed by MAC address."""

    client: dict[MacAddress, f_wlan_device_service.QueryIfaceResponse]
    """Client WLAN interfaces keyed by MAC address."""
    ap: dict[MacAddress, f_wlan_device_service.QueryIfaceResponse]
    """AP WLAN interfaces keyed by MAC address."""


class InformationElementType(enum.IntEnum):
    """Information Element type.

    As defined by IEEE 802.11-1997 Section 7.3.2 and further expanded by
    802.11d, 802.11g, 802.11h, and 802.11i.

    https://www.oreilly.com/library/view/80211-wireless-networks/0596100523/ch04.html#wireless802dot112-CHP-4-TABLE-7
    """

    SSID = 0
    # Types 1-255 are not implemented. Only implement a new type if it is being used.


class BssDescriptionParser:
    """BssDescription with parsed information elements."""

    @staticmethod
    def ssid(bss_description: f_wlan_ieee80211.BssDescription) -> str | None:
        """Parse information elements for SSID."""
        ies = bytes(bss_description.ies)
        i = 0
        while i < len(ies):
            if not len(ies) > i + 1:
                raise TypeError(
                    "Invalid information element; requires at least 2 bytes for "
                    f"Element ID and Length, got {len(ies) - i}"
                )

            element = int(ies[i])
            length = int(ies[i + 1])
            i += 2

            try:
                ie_type = InformationElementType(int(element))
            except ValueError:
                # Type not implemented. It's okay to skip
                i += length
                continue

            match ie_type:
                case InformationElementType.SSID:
                    try:
                        return ies[i : i + length].decode("utf-8")
                    except UnicodeDecodeError:
                        # ssid is not valid UTF-8; fallback to counting bytes
                        return f"<ssid-{length}>"
                case _:
                    raise TypeError(
                        f"Unsupported InformationElementType: {ie_type}"
                    )

        return None


class CountryCode:
    """Country codes used for configuring WLAN."""

    _code: bytes

    def __init__(self, code: str | bytes | bytearray) -> None:
        if isinstance(code, str):
            code_bytes = code.encode("ascii")
        else:
            code_bytes = bytes(code)

        if len(code_bytes) != 2:
            raise ValueError(
                f"Expected exactly 2 ASCII bytes, got {len(code_bytes)}"
            )

        self._code = code_bytes

    def __bytes__(self) -> bytes:
        return self._code

    def __str__(self) -> str:
        return self._code.decode("ascii")

    def __repr__(self) -> str:
        return f"CountryCode('{self}')"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, CountryCode):
            return False
        return self._code == other._code

    def __hash__(self) -> int:
        return hash(self._code)


KNOWN_COUNTRY_CODES = {
    "AUSTRIA": CountryCode("AT"),
    "AUSTRALIA": CountryCode("AU"),
    "BELGIUM": CountryCode("BE"),
    "BULGARIA": CountryCode("BG"),
    "CANADA": CountryCode("CA"),
    "SWITZERLAND": CountryCode("CH"),
    "CHILE": CountryCode("CL"),
    "COLOMBIA": CountryCode("CO"),
    "CYPRUS": CountryCode("CY"),
    "CZECHIA": CountryCode("CZ"),
    "GERMANY": CountryCode("DE"),
    "DENMARK": CountryCode("DK"),
    "ESTONIA": CountryCode("EE"),
    "GREECE_EU": CountryCode("EL"),
    "SPAIN": CountryCode("ES"),
    "FINLAND": CountryCode("FI"),
    "FRANCE": CountryCode("FR"),
    "UNITED_KINGDOM_OF_GREAT_BRITAIN": CountryCode("GB"),
    "GREECE": CountryCode("GR"),
    "CROATIA": CountryCode("HR"),
    "HUNGARY": CountryCode("HU"),
    "IRELAND": CountryCode("IE"),
    "INDIA": CountryCode("IN"),
    "ICELAND": CountryCode("IS"),
    "ITALY": CountryCode("IT"),
    "JAPAN": CountryCode("JP"),
    "KOREA": CountryCode("KR"),
    "LIECHTENSTEIN": CountryCode("LI"),
    "LITHUANIA": CountryCode("LT"),
    "LUXEMBOURG": CountryCode("LU"),
    "LATVIA": CountryCode("LV"),
    "MALTA": CountryCode("MT"),
    "MEXICO": CountryCode("MX"),
    "NETHERLANDS": CountryCode("NL"),
    "NORWAY": CountryCode("NO"),
    "NEW_ZEALAND": CountryCode("NZ"),
    "PERU": CountryCode("PE"),
    "POLAND": CountryCode("PL"),
    "PORTUGAL": CountryCode("PT"),
    "ROMANIA": CountryCode("RO"),
    "SWEDEN": CountryCode("SE"),
    "SINGAPORE": CountryCode("SG"),
    "SLOVENIA": CountryCode("SI"),
    "SLOVAKIA": CountryCode("SK"),
    "TURKEY": CountryCode("TR"),
    "TAIWAN": CountryCode("TW"),
    "UNITED_STATES_OF_AMERICA": CountryCode("US"),
    "USER_XZ": CountryCode("XZ"),
    "WORLDWIDE_ZEROES": CountryCode("00"),
}
