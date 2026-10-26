# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Frozen data models for the driver-lab host tooling.

Models are immutable and validated at construction. Conversions to and
from serialized forms happen in explicit functions; unvalidated
dictionaries are never passed through the system.
"""

import dataclasses
import enum
import hashlib


class AccessClass(enum.Enum):
    """Access classes; a grant for one class never authorizes another."""

    READ_ONCE = "read_once"
    SNAPSHOT = "snapshot"
    POLL = "poll"
    WRITE = "write"
    SEQUENCE = "sequence"
    PROTOCOL = "protocol"
    PROTOCOL_TRANSACTION = "protocol_transaction"
    INTERRUPT = "interrupt"


class Decision(enum.Enum):
    """Persistent grant decision."""

    ALLOW = "allow"
    DENY = "deny"


def _check_common(resource_digest: str, offset: int, width: int) -> None:
    if not resource_digest.startswith("sha256:"):
        raise ValueError("resource_digest must be a sha256:<hex> digest")
    if offset < 0:
        raise ValueError("offset must be non-negative")
    if width <= 0:
        raise ValueError("width must be positive")


@dataclasses.dataclass(frozen=True)
class AccessRequest:
    """One register access whose consent must be resolved."""

    target_scope: str
    node_id: str
    resource_digest: str
    resource: str
    offset: int
    width: int
    access: AccessClass

    def __post_init__(self) -> None:
        _check_common(self.resource_digest, self.offset, self.width)

    @property
    def match_key(self) -> tuple[str, str, str, str, int, int, AccessClass]:
        """The exact identity a grant must match."""
        return (
            self.target_scope,
            self.node_id,
            self.resource_digest,
            self.resource,
            self.offset,
            self.width,
            self.access,
        )


@dataclasses.dataclass(frozen=True)
class ReadGrant:
    """A persistent operator consent rule.

    A grant matches on stable identity criteria only: target scope, node
    identity criteria, resource-description digest, logical resource,
    offset, width, and access class. It never matches solely by nodename,
    boot ID, or devfs path. A boot change alone does not invalidate a
    grant (no boot identity is recorded); a resource-description or
    per-resource policy change does, because the digest changes.
    """

    schema_version: int
    target_scope: str
    node_id: str
    resource_digest: str
    resource: str
    offset: int
    width: int
    access: AccessClass
    decision: Decision
    approved_at: str
    approval_source: str | None = None
    reason: str | None = None
    max_poll_hz: float | None = None
    max_poll_timeout_s: float | None = None

    def __post_init__(self) -> None:
        if self.schema_version != 1:
            raise ValueError(
                f"unsupported grant schema version {self.schema_version}"
            )
        _check_common(self.resource_digest, self.offset, self.width)
        if self.access in (AccessClass.WRITE, AccessClass.SEQUENCE):
            raise ValueError(
                f"persistent {self.access.value} grants are not supported"
            )

        has_poll_limits = (
            self.max_poll_hz is not None or self.max_poll_timeout_s is not None
        )
        if self.access is not AccessClass.POLL and has_poll_limits:
            raise ValueError("poll limits are only valid on poll grants")
        if (
            self.access is AccessClass.POLL
            and self.decision is Decision.ALLOW
            and (self.max_poll_hz is None or self.max_poll_timeout_s is None)
        ):
            raise ValueError(
                "a poll allow grant requires max_poll_hz and max_poll_timeout_s"
            )

    @property
    def match_key(self) -> tuple[str, str, str, str, int, int, AccessClass]:
        """The exact identity this grant matches."""
        return (
            self.target_scope,
            self.node_id,
            self.resource_digest,
            self.resource,
            self.offset,
            self.width,
            self.access,
        )

    @property
    def grant_id(self) -> str:
        """Stable identifier for list/explain/revoke operations."""
        material = "|".join(
            [
                self.target_scope,
                self.node_id,
                self.resource_digest,
                self.resource,
                f"{self.offset:#x}",
                str(self.width),
                self.access.value,
                self.decision.value,
            ]
        )
        return "grant-" + hashlib.sha256(material.encode()).hexdigest()[:16]

    def matches(self, request: AccessRequest) -> bool:
        """Whether this grant exactly matches `request`."""
        return self.match_key == request.match_key


@dataclasses.dataclass(frozen=True)
class WritePrecondition:
    """A required 32-bit register value prior to a write."""

    expected: int
    mask: int = 0xFFFF_FFFF

    def __post_init__(self) -> None:
        if not (0 <= self.expected <= 0xFFFF_FFFF):
            raise ValueError("expected must be a 32-bit unsigned integer")
        if not (0 <= self.mask <= 0xFFFF_FFFF):
            raise ValueError("mask must be a 32-bit unsigned integer")


class ResourceKind(str, enum.Enum):
    """Resource kind identifying MMIO registers or peripheral protocol endpoints."""

    MMIO = "mmio"
    GPIO = "gpio"
    I2C = "i2c"
    SPI = "spi"
    INTERRUPT = "interrupt"
    CLOCK = "clock"
    RESET = "reset"
    SERIAL = "serial"


@dataclasses.dataclass(frozen=True)
class GpioReadOutcome:
    """The outcome of a policy-checked, audited GPIO read."""

    value: bool
    audit_seq: int
    timestamp_ns: int


@dataclasses.dataclass(frozen=True)
class GpioWriteOutcome:
    """The outcome of a policy-checked, audited GPIO write."""

    audit_seq: int
    timestamp_ns: int


@dataclasses.dataclass(frozen=True)
class I2cTransferOutcome:
    """The outcome of a policy-checked, audited I2C transfer."""

    read_data: bytes
    audit_seq: int
    timestamp_ns: int


@dataclasses.dataclass(frozen=True)
class SpiTransmitOutcome:
    """The outcome of a policy-checked, audited SPI transmit."""

    rx_data: bytes
    audit_seq: int
    timestamp_ns: int


@dataclasses.dataclass(frozen=True)
class InterruptOutcome:
    """The outcome of an interrupt observation."""

    resource: int
    sequence: int
    count: int
    timestamp_ns: int
    coalesced_count: int = 0

    def __eq__(self, other: object) -> bool:
        if isinstance(other, int):
            return self.timestamp_ns == other
        if isinstance(other, InterruptOutcome):
            return (
                self.resource == other.resource
                and self.sequence == other.sequence
                and self.count == other.count
                and self.timestamp_ns == other.timestamp_ns
                and self.coalesced_count == other.coalesced_count
            )
        return False

    def __int__(self) -> int:
        return self.timestamp_ns

    def __index__(self) -> int:
        return self.timestamp_ns
