# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Data types used by wlan affordance."""

from __future__ import annotations

import enum
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Protocol, Self

import fidl_fuchsia_wlan_device_service as f_wlan_device_service
import fidl_fuchsia_wlan_ieee80211 as f_wlan_ieee80211
import fidl_fuchsia_wlan_policy as f_wlan_policy
import fidl_fuchsia_wlan_sme as f_wlan_sme

from honeydew.typing.custom_types import MacAddress as _MacAddress

MacAddress = _MacAddress

# Length of a pre-shared key (PSK) used as a password.
_PSK_LENGTH = 64


@dataclass(frozen=True)
class NetworkConfig:
    """Network information used to establish a connection.

    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.wlan.policy/types.fidl
    """

    ssid: str
    security_type: f_wlan_policy.SecurityType
    credential_type: str
    credential_value: str

    @staticmethod
    def from_fidl(fidl: f_wlan_policy.NetworkConfig) -> "NetworkConfig":
        """Parse from a fuchsia.wlan.policy/NetworkConfig."""
        assert fidl.id_ is not None, f"{fidl!r} missing id"
        assert fidl.credential is not None, f"{fidl!r} missing credential"
        identifier = NetworkIdentifier.from_fidl(fidl.id_)
        credential = Credential.from_fidl(fidl.credential)
        return NetworkConfig(
            ssid=identifier.ssid,
            security_type=identifier.security_type,
            credential_type=credential.type(),
            credential_value=credential.value(),
        )

    def to_fidl(self) -> f_wlan_policy.NetworkConfig:
        """Convert to equivalent FIDL."""
        return f_wlan_policy.NetworkConfig(
            id_=NetworkIdentifier(self.ssid, self.security_type).to_fidl(),
            credential=Credential.from_password(
                self.credential_value
            ).to_fidl(),
        )

    def __lt__(self, other: NetworkConfig) -> bool:
        return self.ssid < other.ssid


@dataclass(frozen=True)
class NetworkIdentifier:
    """Combination of ssid and the security type.

    Primary means of distinguishing between available networks.
    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.wlan.policy/types.fidl
    """

    ssid: str
    security_type: f_wlan_policy.SecurityType

    @staticmethod
    def from_fidl(fidl: f_wlan_policy.NetworkIdentifier) -> "NetworkIdentifier":
        """Parse from a fuchsia.wlan.policy/NetworkIdentifier."""

        return NetworkIdentifier(
            ssid=bytes(fidl.ssid).decode("utf-8"),
            security_type=f_wlan_policy.SecurityType(fidl.type_),
        )

    def to_fidl(self) -> f_wlan_policy.NetworkIdentifier:
        """Convert to a fuchsia.wlan.policy/NetworkIdentifier."""
        return f_wlan_policy.NetworkIdentifier(
            ssid=list(self.ssid.encode("utf-8")),
            type_=self.security_type,
        )

    def __lt__(self, other: NetworkIdentifier) -> bool:
        return self.ssid < other.ssid


class Credential(Protocol):
    """Information used to verify access to a target network."""

    def type(self) -> str:
        """Type of credential."""

    def value(self) -> str:
        """Value of the credential, or empty string if not applicable."""

    def to_fidl(self) -> f_wlan_policy.Credential:
        """Convert to a fuchsia.wlan.policy/Credential."""

    @staticmethod
    def from_password(password: str | None) -> Credential:
        """Parse a password into a Credential.

        Args:
            password: String password, pre-shared key in hex form with length 64, or
                None/empty to represent open.

        Return:
            A fuchsia.wlan.policy/Credential union object.
        """
        if not password:
            return CredentialNone()
        elif len(password) == _PSK_LENGTH:
            return CredentialPsk(password)
        else:
            return CredentialPassword(password)

    @staticmethod
    def from_fidl(fidl: f_wlan_policy.Credential) -> Credential:
        """Parse a fuchsia.wlan.policy/Credential."""
        if fidl.none is not None:
            return CredentialNone()
        if fidl.password is not None:
            return CredentialPassword(bytes(fidl.password).decode("utf-8"))
        if fidl.psk is not None:
            return CredentialPsk(bytes(fidl.psk).hex())
        raise TypeError(
            f"Unknown value for fuchsia.wlan.policy/Credential: {fidl}"
        )


class CredentialNone(Credential):
    """Credentials to connect to an unprotected network."""

    def type(self) -> str:
        return "None"

    def value(self) -> str:
        return ""

    def to_fidl(self) -> f_wlan_policy.Credential:
        cred = f_wlan_policy.Credential(none=f_wlan_policy.Empty())
        return cred


@dataclass(frozen=True)
class CredentialPassword(Credential):
    """Credentials to connect to an password protected network."""

    password: str
    """Plaintext password."""

    def type(self) -> str:
        return "Password"

    def value(self) -> str:
        return self.password

    def to_fidl(self) -> f_wlan_policy.Credential:
        cred = f_wlan_policy.Credential(
            password=list(self.password.encode("utf-8"))
        )
        return cred


@dataclass(frozen=True)
class CredentialPsk(Credential):
    """Credentials to connect to an network using a pre-shared key."""

    psk: str
    """Hash representation of the network passphrase."""

    def type(self) -> str:
        return "Psk"

    def value(self) -> str:
        return self.psk

    def to_fidl(self) -> f_wlan_policy.Credential:
        cred = f_wlan_policy.Credential(psk=list(bytes.fromhex(self.psk)))
        return cred


@dataclass(frozen=True)
class NetworkState:
    """Information about a network's current connections and attempts.

    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.wlan.policy/client_provider.fidl
    """

    network_identifier: NetworkIdentifier
    connection_state: f_wlan_policy.ConnectionState
    disconnect_status: f_wlan_policy.DisconnectStatus | None

    @staticmethod
    def from_fidl(fidl: f_wlan_policy.NetworkState) -> "NetworkState":
        """Parse from a fuchsia.wlan.policy/NetworkState."""
        assert fidl.id_ is not None, f"{fidl!r} missing id"
        assert fidl.state is not None, f"{fidl!r} missing state"

        return NetworkState(
            network_identifier=NetworkIdentifier.from_fidl(fidl.id_),
            connection_state=f_wlan_policy.ConnectionState(fidl.state),
            disconnect_status=(
                f_wlan_policy.DisconnectStatus(fidl.status)
                if fidl.status
                else None
            ),
        )

    def __lt__(self, other: NetworkState) -> bool:
        return self.network_identifier < other.network_identifier


@dataclass(frozen=True)
class ClientStateSummary:
    """Information about the current client state for the device.

    This includes if the device will attempt to connect to access points
    (when applicable), any existing connections and active connection attempts
    and their outcomes.
    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/fidl/fuchsia.wlan.policy/client_provider.fidl
    """

    state: f_wlan_policy.WlanClientState
    networks: list[NetworkState]

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.ClientStateSummary,
    ) -> "ClientStateSummary":
        """Parse from a fuchsia.wlan.policy/ClientStateSummary."""
        assert fidl.networks is not None, f"{fidl!r} missing networks"
        assert fidl.state is not None, f"{fidl!r} missing state"
        return ClientStateSummary(
            state=f_wlan_policy.WlanClientState(fidl.state),
            networks=[NetworkState.from_fidl(n) for n in fidl.networks],
        )

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, ClientStateSummary):
            return NotImplemented
        return self.state == other.state and sorted(self.networks) == sorted(
            other.networks
        )


@dataclass(frozen=True)
class WlanChannel:
    """Wlan channel information.

    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:src/testing/sl4f/src/wlan/types.rs
    """

    primary: int
    cbw: f_wlan_ieee80211.ChannelBandwidth
    secondary80: int

    @staticmethod
    def from_fidl(fidl: f_wlan_ieee80211.WlanChannel) -> "WlanChannel":
        """Parse from a fuchsia.wlan.common/WlanChannel."""
        return WlanChannel(
            primary=fidl.primary,
            cbw=f_wlan_ieee80211.ChannelBandwidth(fidl.cbw),
            secondary80=fidl.secondary80,
        )

    def to_fidl(self) -> f_wlan_ieee80211.WlanChannel:
        """Convert to a fuchsia.wlan.common/WlanChannel."""
        return f_wlan_ieee80211.WlanChannel(
            primary=self.primary,
            cbw=self.cbw,
            secondary80=self.secondary80,
        )


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


# TODO(http://b/346424966): Only necessary because Python does not have static
# typing for FIDL. Once these static types are available and the SL4F affordance
# is removed, replace with the statically generated FIDL equivalent.
class ClientStatusResponse(Protocol):
    """WLAN client interface status."""

    def status(self) -> str:
        """Description of the client's status."""

    @staticmethod
    def from_fidl(
        fidl: f_wlan_sme.ClientStatusResponse,
    ) -> "ClientStatusResponse":
        """Parse from a fuchsia.wlan.sme/ClientStatusResponse."""
        if fidl.connected:
            ap: f_wlan_sme.ServingApInfo = fidl.connected
            return ClientStatusConnected(
                bssid=list(ap.bssid),
                ssid=list(ap.ssid),
                rssi_dbm=ap.rssi_dbm,
                snr_db=ap.snr_db,
                channel=WlanChannel.from_fidl(ap.channel),
                protection=f_wlan_sme.Protection(ap.protection),
            )

        if fidl.connecting:
            return ClientStatusConnecting(ssid=fidl.connecting)

        if fidl.idle:
            return ClientStatusIdle()

        raise TypeError(f"Unknown ClientStatusResponse FIDL value: {fidl}")


@dataclass(frozen=True)
class ClientStatusConnected(ClientStatusResponse):
    """ServingApInfo, returned as a part of ClientStatusResponse.

    Defined by https://cs.opensource.google/fuchsia/fuchsia/+/main:src/testing/sl4f/src/wlan/types.rs
    """

    bssid: list[int]
    ssid: list[int]
    rssi_dbm: int
    snr_db: int
    channel: WlanChannel
    protection: f_wlan_sme.Protection

    def status(self) -> str:
        return "Connected"


@dataclass(frozen=True)
class ClientStatusConnecting(ClientStatusResponse):
    ssid: Sequence[int]

    def status(self) -> str:
        return "Connecting"


@dataclass(frozen=True)
class ClientStatusIdle(ClientStatusResponse):
    def status(self) -> str:
        return "Idle"


class CountryCode(enum.StrEnum):
    """Country codes used for configuring WLAN.

    This is a list countries and their respective Alpha-2 codes. It comes from
    http://cs/h/turquoise-internal/turquoise/+/main:src/devices/board/drivers/nelson/nelson-sdio.cc?l=79.
    TODO(http://b/337930095): We need to add the other product specific country code
    lists and test for them.
    """

    AUSTRIA = "AT"
    AUSTRALIA = "AU"
    BELGIUM = "BE"
    BULGARIA = "BG"
    CANADA = "CA"
    SWITZERLAND = "CH"
    CHILE = "CL"
    COLOMBIA = "CO"
    CYPRUS = "CY"
    CZECHIA = "CZ"
    GERMANY = "DE"
    DENMARK = "DK"
    ESTONIA = "EE"
    GREECE_EU = "EL"
    SPAIN = "ES"
    FINLAND = "FI"
    FRANCE = "FR"
    UNITED_KINGDOM_OF_GREAT_BRITAIN = "GB"
    GREECE = "GR"
    CROATIA = "HR"
    HUNGARY = "HU"
    IRELAND = "IE"
    INDIA = "IN"
    ICELAND = "IS"
    ITALY = "IT"
    JAPAN = "JP"
    KOREA = "KR"
    LIECHTENSTEIN = "LI"
    LITHUANIA = "LT"
    LUXEMBOURG = "LU"
    LATVIA = "LV"
    MALTA = "MT"
    MEXICO = "MX"
    NETHERLANDS = "NL"
    NORWAY = "NO"
    NEW_ZEALAND = "NZ"
    PERU = "PE"
    POLAND = "PL"
    PORTUGAL = "PT"
    ROMANIA = "RO"
    SWEDEN = "SE"
    SINGAPORE = "SG"
    SLOVENIA = "SI"
    SLOVAKIA = "SK"
    TURKEY = "TR"
    TAIWAN = "TW"
    UNITED_STATES_OF_AMERICA = "US"
    WORLDWIDE = "WW"
    USER_XZ = "XZ"
    # WW and 00 both refer to worldwide mode
    WORLDWIDE_ZEROES = "00"

    @classmethod
    def from_bytes(cls, data: bytes) -> Self:
        """Create an instance from a 2-byte UTF-8 encoded string."""
        if len(data) != 2:
            raise ValueError(
                f"Expected exactly 2 bytes, got {len(data)}: {data!r}"
            )

        try:
            code_str = data.decode("utf-8")
        except UnicodeDecodeError as e:
            raise ValueError(f"Bytes {data!r} are not valid UTF-8") from e

        return cls(code_str)


class ConnectivityMode(enum.IntEnum):
    """Connectivity operating mode for the access point."""

    LOCAL_ONLY = 1
    """Allows for connectivity between co-located devices.

    Local only access points do not forward traffic to other network connections.
    """

    UNRESTRICTED = 2
    """Allows for full connectivity.

    Traffic can potentially being forwarded to other network connections (e.g.
    tethering mode).
    """

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.ConnectivityMode,
    ) -> "ConnectivityMode":
        """Parse from a fuchsia.wlan.policy/ConnectivityMode."""
        return ConnectivityMode(fidl)

    def to_fidl(self) -> f_wlan_policy.ConnectivityMode:
        """Convert to equivalent FIDL."""
        return f_wlan_policy.ConnectivityMode(self.value)


class OperatingBand(enum.IntEnum):
    """Operating band for wlan control request and status updates."""

    ANY = 1
    """Allows for band switching depending on device operating mode and
    environment."""

    ONLY_2_4GHZ = 2
    """Restricted to 2.4 GHz bands only."""

    ONLY_5GHZ = 3
    """Restricted to 5 GHz bands only."""

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.OperatingBand,
    ) -> "OperatingBand":
        """Parse from a fuchsia.wlan.policy/OperatingBand."""
        return OperatingBand(fidl)

    def to_fidl(self) -> f_wlan_policy.OperatingBand:
        """Convert to equivalent FIDL."""
        return f_wlan_policy.OperatingBand(self.value)


class OperatingState(enum.IntEnum):
    """Current detailed operating state for an access point."""

    FAILED = 1
    """Access point operation failed.

    Access points that enter the failed state will have one update informing
    registered listeners of the failure and then an additional update with the
    access point removed from the list.
    """

    STARTING = 2
    """Access point operation is starting up."""

    ACTIVE = 3
    """Access point operation is active."""

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.OperatingState,
    ) -> "OperatingState":
        """Parse from a fuchsia.wlan.policy/OperatingState."""
        return OperatingState(fidl)


@dataclass(frozen=True)
class AccessPointState:
    """Information about the individual operating access points.

    This includes limited information about any connected clients.
    """

    state: OperatingState
    """Current access point operating state."""

    mode: ConnectivityMode
    """Requested operating connectivity mode."""

    band: OperatingBand
    """Access point operating band."""

    frequency: int | None
    """Access point operating frequency (in MHz)."""

    clients: ConnectedClientInformation | None
    """Information about connected clients."""

    id_: NetworkIdentifier
    """Identifying information of the access point whose state has changed."""

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.AccessPointState,
    ) -> "AccessPointState":
        """Parse from a fuchsia.wlan.policy/AccessPointState."""
        assert fidl.state is not None, f"{fidl!r} missing state"
        assert fidl.mode is not None, f"{fidl!r} missing mode"
        assert fidl.band is not None, f"{fidl!r} missing band"
        assert fidl.id_ is not None, f"{fidl!r} missing id"

        return AccessPointState(
            state=OperatingState.from_fidl(
                f_wlan_policy.OperatingState(fidl.state)
            ),
            mode=ConnectivityMode.from_fidl(
                f_wlan_policy.ConnectivityMode(fidl.mode)
            ),
            band=OperatingBand.from_fidl(
                f_wlan_policy.OperatingBand(fidl.band)
            ),
            frequency=fidl.frequency,
            clients=(
                ConnectedClientInformation.from_fidl(fidl.clients)
                if fidl.clients
                else None
            ),
            id_=NetworkIdentifier.from_fidl(fidl.id_),
        )


@dataclass(frozen=True)
class ConnectedClientInformation:
    """Connected client information.

    This is initially limited to the number of connected clients.
    """

    count: int
    """Number of connected clients."""

    @staticmethod
    def from_fidl(
        fidl: f_wlan_policy.ConnectedClientInformation,
    ) -> "ConnectedClientInformation":
        """Parse from a fuchsia.wlan.policy/ConnectedClientInformation."""
        assert fidl.count is not None, f"{fidl!r} missing count"
        return ConnectedClientInformation(count=fidl.count)
