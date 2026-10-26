# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Driver-shaped public Python programming model (Spec Sections 6.1, 9; Milestone H4).

Provides ergonomic, capability-aware access to hardware resources on Fuchsia targets.
Preserves production FIDL method boundaries, MMIO access width and ordering,
asynchronous waits, and the distinction between semantic device protocols and
raw registers.
"""

import asyncio
import dataclasses
from collections.abc import Awaitable, Callable, Mapping, Sequence
from typing import Any

from driver_lab.models import InterruptOutcome, ResourceKind
from driver_lab.transport import (
    Denial,
    DirectDescription,
    DirectSession,
    FidlCallOutcome,
    OperationDenied,
    PollOutcome,
    ProxyDescription,
    ProxySession,
    ResourceInfo,
    SequenceItem,
    SequenceOutcome,
    SnapshotItem,
    WriteOutcome,
)

# Aliases matching both spec and transport terminology
WriteResult = WriteOutcome
PollResult = PollOutcome


class UnsupportedCapabilityError(Exception):
    """Raised when an operation requires capabilities not supported by the session mode."""


@dataclasses.dataclass(frozen=True)
class TranslationMetadata:
    """Documents C++ and Rust driver analogues for host Python operations (Spec Section 9.1)."""

    cpp_analogue: str
    rust_analogue: str
    directly_translatable: bool
    differences: str
    target_local_timing: bool
    experiment_only: bool = False


@dataclasses.dataclass(frozen=True)
class SessionCapabilities:
    """Explicitly reported session capabilities and degradation guarantees (Spec Section 6.2)."""

    mode: str  # "proxy" or "direct"
    target_policy: bool
    target_audit: bool
    target_local_timing: bool
    fault_isolation: str  # "driver_host" | "none" | "process"
    production_driver_active: bool
    restoration_required: bool = False
    resources: tuple[str, ...] = ()
    protocols: tuple[str, ...] = ()

    def check_support(self, feature: str) -> None:
        """Fails closed if the requested feature is not supported by the current session mode."""
        if feature == "mmio" and self.mode not in ("proxy", "in-situ"):
            raise UnsupportedCapabilityError(
                f"MMIO register operations are only supported in proxy or in-situ mode (current mode: {self.mode})"
            )
        if feature == "sequence" and not self.target_local_timing:
            raise UnsupportedCapabilityError(
                "Target-local sequence execution is not supported in direct mode"
            )
        if feature == "target_audit" and not self.target_audit:
            raise UnsupportedCapabilityError(
                "Target audit logging is not supported in direct mode"
            )
        if feature == "target_policy" and not self.target_policy:
            raise UnsupportedCapabilityError(
                "Target policy enforcement is not supported in direct mode"
            )


@dataclasses.dataclass(frozen=True)
class AccessRequirements:
    """Declared requirements for connecting or attaching to a hardware node (Spec Section 7.3)."""

    needs_mmio: bool = False
    needs_sequence: bool = False
    needs_target_timing: bool = False
    needs_target_policy: bool = False
    needs_target_audit: bool = False
    is_mutating: bool = False
    protocol: str | None = None


class MmioRegion:
    """Driver-shaped interface to a memory-mapped I/O register block (Spec Sections 6.1, 9).

    Provides typed 32-bit register reads, masked writes with preconditions and readback,
    polling, and bounded snapshots.
    """

    read32_metadata = TranslationMetadata(
        cpp_analogue="mmio.Read32(offset) / fdf::MmioBuffer::Read32(offset)",
        rust_analogue="mmio.read32(offset)",
        directly_translatable=True,
        differences="Executed over FIDL proxy channel with host round-trip latency and target audit logging.",
        target_local_timing=False,
        experiment_only=False,
    )

    write32_metadata = TranslationMetadata(
        cpp_analogue="mmio.Write32(value, offset) / mmio.ModifyBits32(value, mask, offset)",
        rust_analogue="mmio.write32(offset, value) / mmio.modify32(offset, ...)",
        directly_translatable=True,
        differences="Executed over FIDL proxy channel with host round trip, optional precondition check, and automatic readback.",
        target_local_timing=False,
        experiment_only=False,
    )

    poll32_metadata = TranslationMetadata(
        cpp_analogue="hwreg::RegisterAddr<...>::ReadFrom(&mmio).Poll(...) or loop with zx::nanosleep",
        rust_analogue="polling loop with fuchsia_async::Timer",
        directly_translatable=False,
        differences="Target-local polling in proxy driver dispatcher without host round trips per attempt.",
        target_local_timing=True,
        experiment_only=False,
    )

    snapshot32_metadata = TranslationMetadata(
        cpp_analogue="bounded read loop",
        rust_analogue="bounded read loop",
        directly_translatable=False,
        differences="Convenience batched read operation for experiment snapshotting.",
        target_local_timing=False,
        experiment_only=True,
    )

    def __init__(
        self,
        resource: ResourceInfo,
        session: ProxySession,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._resource = resource
        self._session = session
        self._audit_drainer = audit_drainer

    @property
    def name(self) -> str:
        """Name of the logical resource."""
        return self._resource.name

    @property
    def id(self) -> int:
        """Numeric ID of the resource."""
        return self._resource.id

    @property
    def logical_size(self) -> int:
        """Logical size in bytes."""
        return self._resource.logical_size

    @property
    def digest(self) -> str:
        """Resource digest."""
        return self._resource.digest

    def _validate_offset(self, offset: int) -> None:
        if offset < 0 or offset + 4 > self._resource.logical_size:
            raise ValueError(
                f"Offset {hex(offset)} out of logical bounds [0, {hex(self._resource.logical_size)}) for {self.name}"
            )
        if offset % 4 != 0:
            raise ValueError(
                f"Misaligned 32-bit register access at offset {hex(offset)} (must be 4-byte aligned)"
            )

    async def read32(self, offset: int) -> int:
        """Reads a 32-bit register at byte offset."""
        self._validate_offset(offset)
        outcome = await self._session.read32(self.id, offset)
        return outcome.value

    async def write32(
        self,
        offset: int,
        value: int,
        *,
        mask: int = 0xFFFF_FFFF,
        expected_before: int | None = None,
        expected_mask: int | None = 0xFFFF_FFFF,
        require_readback: bool = True,
    ) -> WriteOutcome:
        """Writes a 32-bit register with optional mask, precondition, and readback."""
        self._validate_offset(offset)
        precondition_tuple: tuple[int, int] | None = None
        if expected_before is not None:
            precondition_tuple = (
                expected_before,
                expected_mask if expected_mask is not None else 0xFFFF_FFFF,
            )
        outcome = await self._session.write32(
            resource=self.id,
            offset=offset,
            value=value,
            write_mask=mask,
            precondition=precondition_tuple,
            readback=require_readback,
        )
        if self._audit_drainer is not None:
            await self._audit_drainer()
        return outcome

    async def poll32(
        self,
        offset: int,
        *,
        expected: int,
        mask: int = 0xFFFF_FFFF,
        interval_s: float = 0.001,
        timeout_s: float = 1.0,
    ) -> PollOutcome:
        """Polls a 32-bit register until (value & mask) == (expected & mask) or timeout."""
        self._validate_offset(offset)
        interval_ns = max(1, int(interval_s * 1e9))
        timeout_ns = max(1, int(timeout_s * 1e9))
        outcome = await self._session.poll32(
            resource=self.id,
            offset=offset,
            expected=expected,
            mask=mask,
            interval_ns=interval_ns,
            timeout_ns=timeout_ns,
        )
        return outcome

    async def snapshot32(self, offsets: Sequence[int]) -> list[int]:
        """Reads multiple 32-bit registers in a single bounded batch."""
        for off in offsets:
            self._validate_offset(off)
        items = [SnapshotItem(resource=self.id, offset=off) for off in offsets]
        outcome = await self._session.snapshot(items)
        if not outcome.complete:
            raise OperationDenied(Denial.BACKEND_FAULT)
        return [r.value for r in outcome.results if r.ok]


class StateHandle:
    """Driver-shaped interface to a software state, runtime knob, and trigger bank (Phase 3 / CS32).

    Wraps an underlying 32-bit `StateBank` resource (`MmioRegion`) with semantic methods for:
    - `.read32(offset)` for reading state slots and read probes
    - `.poll32(offset, expected, mask)` for polling software state transitions
    - `.write_knob(offset, value)` for tuning runtime parameters with readback
    - `.trigger(offset, arg=1)` for invoking deterministic self-tests and reading back their result
    """

    read32_metadata = TranslationMetadata(
        cpp_analogue="state_slot.load(std::memory_order_seq_cst)",
        rust_analogue="state_slot.load()",
        directly_translatable=False,
        differences="In-situ host observation of a driver StateBank slot or read probe over FIDL.",
        target_local_timing=False,
        experiment_only=True,
    )

    write_knob_metadata = TranslationMetadata(
        cpp_analogue="knob_slot.store(value, std::memory_order_seq_cst)",
        rust_analogue="knob_slot.store(value)",
        directly_translatable=False,
        differences="In-situ host write to a driver StateBank runtime tuning knob with readback.",
        target_local_timing=False,
        experiment_only=True,
    )

    trigger_metadata = TranslationMetadata(
        cpp_analogue="trigger_callback(arg)",
        rust_analogue="trigger_callback(arg) -> Result<u32, BackendError>",
        directly_translatable=False,
        differences="In-situ host invocation of a synchronous driver StateBank trigger callback returning its status via readback.",
        target_local_timing=True,
        experiment_only=True,
    )

    def __init__(self, region: MmioRegion) -> None:
        self._region = region

    @property
    def name(self) -> str:
        """Name of the logical state bank resource."""
        return self._region.name

    @property
    def id(self) -> int:
        """Numeric ID of the state bank resource."""
        return self._region.id

    @property
    def logical_size(self) -> int:
        """Logical size in bytes."""
        return self._region.logical_size

    @property
    def digest(self) -> str:
        """Resource digest."""
        return self._region.digest

    async def read32(self, offset: int) -> int:
        """Reads a 32-bit software state slot or read probe at byte offset."""
        return await self._region.read32(offset)

    async def poll32(
        self,
        offset: int,
        expected: int = 0,
        mask: int = 0xFFFF_FFFF,
        *,
        interval_s: float = 0.001,
        timeout_s: float = 1.0,
    ) -> PollOutcome:
        """Polls a 32-bit software state slot until (value & mask) == (expected & mask) or timeout."""
        return await self._region.poll32(
            offset,
            expected=expected,
            mask=mask,
            interval_s=interval_s,
            timeout_s=timeout_s,
        )

    async def write_knob(
        self,
        offset: int,
        value: int,
        *,
        mask: int = 0xFFFF_FFFF,
        expected_before: int | None = None,
        expected_mask: int | None = 0xFFFF_FFFF,
        require_readback: bool = True,
    ) -> WriteOutcome:
        """Writes a 32-bit runtime tuning knob with automatic readback."""
        return await self._region.write32(
            offset,
            value,
            mask=mask,
            expected_before=expected_before,
            expected_mask=expected_mask,
            require_readback=require_readback,
        )

    async def trigger(
        self,
        offset: int,
        arg: int = 1,
        *,
        mask: int = 0xFFFF_FFFF,
        expected_before: int | None = None,
        expected_mask: int | None = 0xFFFF_FFFF,
        require_readback: bool = False,
    ) -> WriteOutcome:
        """Invokes a deterministic trigger slot at `offset` with `arg` without performing an extra readback."""
        return await self._region.write32(
            offset,
            arg,
            mask=mask,
            expected_before=expected_before,
            expected_mask=expected_mask,
            require_readback=require_readback,
        )

    async def snapshot32(self, offsets: Sequence[int]) -> list[int]:
        """Reads multiple 32-bit state slots in a single bounded batch."""
        return await self._region.snapshot32(offsets)


class ProtocolProxy:
    """Direct published FIDL protocol client adapter."""

    def __init__(self, session: DirectSession, protocol_name: str) -> None:
        self._session = session
        self._protocol_name = protocol_name

    @property
    def protocol_name(self) -> str:
        return self._protocol_name

    async def call(
        self, method: str, args: Mapping[str, object] | None = None
    ) -> FidlCallOutcome:
        """Invokes a method on the published protocol."""
        return await self._session.call_fidl(method, args)

    def __getattr__(self, name: str) -> Any:
        async def _caller(**kwargs: object) -> FidlCallOutcome:
            return await self.call(name, kwargs)

        return _caller


class Gpio:
    """Thin shape-preserving adapter for GPIO pin protocol (Spec Sections 8.1, 9.5, 12)."""

    read_metadata = TranslationMetadata(
        cpp_analogue="gpio.Read() / ddk::GpioProtocolClient::Read() / fuchsia::hardware::pin::Pin::Read()",
        rust_analogue="gpio.read().await / fuchsia_hardware_pin::PinProxy::read()",
        directly_translatable=True,
        differences="Target-side GPIO read mediated by proxy driver with audit log entry and policy check.",
        target_local_timing=False,
        experiment_only=False,
    )

    write_metadata = TranslationMetadata(
        cpp_analogue="gpio.Write(value) / ddk::GpioProtocolClient::Write(value) / fuchsia::hardware::pin::Pin::SetBufferMode()",
        rust_analogue="gpio.write(value).await / fuchsia_hardware_pin::PinProxy::set_buffer_mode()",
        directly_translatable=True,
        differences="Target-side GPIO write mediated by proxy driver with audit log entry and policy check.",
        target_local_timing=False,
        experiment_only=False,
    )

    def __init__(
        self,
        session: DirectSession | ProxySession,
        resource_name: str = "gpio",
        resource_info: ResourceInfo | None = None,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._session = session
        self._resource_name = resource_name
        self._resource_info = resource_info
        self._audit_drainer = audit_drainer

    @property
    def name(self) -> str:
        return self._resource_name

    @property
    def id(self) -> int | None:
        return (
            self._resource_info.id if self._resource_info is not None else None
        )

    async def read(self) -> bool:
        if self._resource_info is not None and hasattr(
            self._session, "gpio_read"
        ):
            outcome = await self._session.gpio_read(self._resource_info.id)
            if self._audit_drainer is not None:
                await self._audit_drainer()
            return outcome.value
        if hasattr(self._session, "call_fidl"):
            outcome = await self._session.call_fidl(
                "read", {"resource": self._resource_name}
            )
            return bool(outcome.response.get("value", False))
        raise UnsupportedCapabilityError("Session does not support GPIO read")

    async def write(self, value: bool) -> None:
        if self._resource_info is not None and hasattr(
            self._session, "gpio_write"
        ):
            await self._session.gpio_write(self._resource_info.id, value)
            if self._audit_drainer is not None:
                await self._audit_drainer()
            return
        if hasattr(self._session, "call_fidl"):
            await self._session.call_fidl(
                "write", {"resource": self._resource_name, "value": value}
            )
            return
        raise UnsupportedCapabilityError("Session does not support GPIO write")

    async def set_direction(self, direction: str) -> None:
        if hasattr(self._session, "call_fidl"):
            await self._session.call_fidl(
                "set_direction",
                {"resource": self._resource_name, "direction": direction},
            )
            return
        raise UnsupportedCapabilityError(
            "GPIO set_direction is only supported in direct mode"
        )


class I2c:
    """Thin shape-preserving adapter for I2C bus protocol (Spec Sections 8.1, 9.5, 12)."""

    transfer_metadata = TranslationMetadata(
        cpp_analogue="i2c.Transfer(...) / fuchsia::hardware::i2c::Device::Transfer()",
        rust_analogue="i2c.transfer(...).await / fuchsia_hardware_i2c::DeviceProxy::transfer()",
        directly_translatable=True,
        differences="I2C transaction executed by target proxy driver via underlying FIDL/DDK protocol with bounded payload and audit logging.",
        target_local_timing=False,
        experiment_only=False,
    )

    def __init__(
        self,
        session: DirectSession | ProxySession,
        resource_name: str = "i2c",
        resource_info: ResourceInfo | None = None,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._session = session
        self._resource_name = resource_name
        self._resource_info = resource_info
        self._audit_drainer = audit_drainer

    @property
    def name(self) -> str:
        return self._resource_name

    @property
    def id(self) -> int | None:
        return (
            self._resource_info.id if self._resource_info is not None else None
        )

    async def transfer(self, write_data: bytes, read_length: int = 0) -> bytes:
        if self._resource_info is not None and hasattr(
            self._session, "i2c_transfer"
        ):
            outcome = await self._session.i2c_transfer(
                self._resource_info.id,
                write_data=write_data,
                read_length=read_length,
            )
            if self._audit_drainer is not None:
                await self._audit_drainer()
            return outcome.read_data
        if hasattr(self._session, "call_fidl"):
            outcome = await self._session.call_fidl(
                "transfer",
                {
                    "resource": self._resource_name,
                    "write_data": list(write_data),
                    "read_length": read_length,
                },
            )
            data = outcome.response.get("read_data", b"")
            if isinstance(data, (bytes, bytearray)):
                return bytes(data)
            if isinstance(data, list):
                return bytes(data)
            return b""
        raise UnsupportedCapabilityError(
            "Session does not support I2C transfer"
        )


class Spi:
    """Thin shape-preserving adapter for SPI bus protocol (Spec Sections 8.1, 9.5, 12)."""

    transmit_metadata = TranslationMetadata(
        cpp_analogue="spi.Transmit(...) / fuchsia::hardware::spi::Device::Transmit()",
        rust_analogue="spi.transmit(...).await / fuchsia_hardware_spi::DeviceProxy::transmit()",
        directly_translatable=True,
        differences="SPI full-duplex / transmit transaction executed by target proxy driver with bounded payload and audit logging.",
        target_local_timing=False,
        experiment_only=False,
    )

    def __init__(
        self,
        session: DirectSession | ProxySession,
        resource_name: str = "spi",
        resource_info: ResourceInfo | None = None,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._session = session
        self._resource_name = resource_name
        self._resource_info = resource_info
        self._audit_drainer = audit_drainer

    @property
    def name(self) -> str:
        return self._resource_name

    @property
    def id(self) -> int | None:
        return (
            self._resource_info.id if self._resource_info is not None else None
        )

    async def transmit(self, tx_data: bytes) -> bytes:
        if self._resource_info is not None and hasattr(
            self._session, "spi_transmit"
        ):
            outcome = await self._session.spi_transmit(
                self._resource_info.id,
                tx_data=tx_data,
            )
            if self._audit_drainer is not None:
                await self._audit_drainer()
            return outcome.rx_data
        if hasattr(self._session, "call_fidl"):
            outcome = await self._session.call_fidl(
                "transmit",
                {
                    "resource": self._resource_name,
                    "tx_data": list(tx_data),
                },
            )
            data = outcome.response.get("rx_data", b"")
            if isinstance(data, (bytes, bytearray)):
                return bytes(data)
            if isinstance(data, list):
                return bytes(data)
            return b""
        raise UnsupportedCapabilityError(
            "Session does not support SPI transmit"
        )


class Serial:
    """Thin shape-preserving adapter for serial stream protocol."""

    def __init__(
        self, session: DirectSession, resource_name: str = "serial"
    ) -> None:
        self._session = session
        self._resource_name = resource_name

    async def write(self, data: bytes) -> int:
        outcome = await self._session.call_fidl(
            "write",
            {"resource": self._resource_name, "data": list(data)},
        )
        val = outcome.response.get("bytes_written")
        if isinstance(val, int):
            return val
        return len(data)

    async def read(self, max_bytes: int) -> bytes:
        outcome = await self._session.call_fidl(
            "read",
            {"resource": self._resource_name, "max_bytes": max_bytes},
        )
        data = outcome.response.get("data", b"")
        if isinstance(data, (bytes, bytearray)):
            return bytes(data)
        if isinstance(data, list):
            return bytes(data)
        return b""


class Clock:
    """Thin shape-preserving adapter for clock control protocol."""

    def __init__(
        self, session: DirectSession, resource_name: str = "clock"
    ) -> None:
        self._session = session
        self._resource_name = resource_name

    async def enable(self) -> None:
        await self._session.call_fidl(
            "enable", {"resource": self._resource_name}
        )

    async def disable(self) -> None:
        await self._session.call_fidl(
            "disable", {"resource": self._resource_name}
        )


class Reset:
    """Thin shape-preserving adapter for reset control protocol."""

    def __init__(
        self, session: DirectSession, resource_name: str = "reset"
    ) -> None:
        self._session = session
        self._resource_name = resource_name

    async def assert_reset(self) -> None:
        await self._session.call_fidl(
            "assert_reset", {"resource": self._resource_name}
        )

    async def deassert_reset(self) -> None:
        await self._session.call_fidl(
            "deassert_reset", {"resource": self._resource_name}
        )


class Interrupt:
    """Thin shape-preserving adapter for interrupt observation (Spec Sections 11.7, 17)."""

    wait_metadata = TranslationMetadata(
        cpp_analogue="zx::interrupt::wait(&timestamp) / fdf::WireAsyncEventHandler",
        rust_analogue="fuchsia_async::OnSignals::new(&interrupt, zx::Signals::INTERRUPT).await",
        directly_translatable=True,
        differences="Bounded hanging request to target proxy with sequence tracking, coalescing metrics, and target monotonic timestamping.",
        target_local_timing=True,
        experiment_only=False,
    )

    def __init__(
        self,
        session: DirectSession | ProxySession,
        resource_name: str = "interrupt",
        resource_info: ResourceInfo | None = None,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._session = session
        self._resource_name = resource_name
        self._resource_info = resource_info
        self._audit_drainer = audit_drainer

    @property
    def name(self) -> str:
        return self._resource_name

    @property
    def id(self) -> int | None:
        return (
            self._resource_info.id if self._resource_info is not None else None
        )

    async def wait(
        self,
        timeout_s: float = 1.0,
        after_sequence: int = 0,
    ) -> InterruptOutcome:
        """Observes an interrupt event (Spec Sections 11.7, 17)."""
        if self._resource_info is not None and isinstance(
            self._session, ProxySession
        ):
            outcome = await self._session.wait_for_interrupt(
                self._resource_info.id,
                after_sequence=after_sequence,
                timeout_s=timeout_s,
            )
            if self._audit_drainer is not None:
                await self._audit_drainer()
            return outcome

        if isinstance(self._session, DirectSession):
            outcome_fidl = await self._session.call_fidl(
                "wait",
                {
                    "resource": self._resource_name,
                    "timeout_ns": int(timeout_s * 1e9),
                },
            )
            val = outcome_fidl.response.get("timestamp_ns")
            ts = val if isinstance(val, int) else 0
            return InterruptOutcome(
                resource=self._resource_info.id if self._resource_info else 0,
                sequence=1,
                count=1,
                timestamp_ns=ts,
                coalesced_count=0,
            )
        raise RuntimeError("Session does not support wait_for_interrupt")


class HardwareSession:
    """Unified driver-shaped session facade for hardware exploration (Spec Sections 6.1, 9)."""

    def __init__(
        self,
        *,
        capabilities: SessionCapabilities,
        proxy_session: ProxySession | None = None,
        direct_session: DirectSession | None = None,
        proxy_description: ProxyDescription | None = None,
        direct_description: DirectDescription | None = None,
        audit_drainer: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._capabilities = capabilities
        self._proxy_session = proxy_session
        self._direct_session = direct_session
        self._proxy_description = proxy_description
        self._direct_description = direct_description
        self._audit_drainer = audit_drainer
        self._closed = False

    @property
    def capabilities(self) -> SessionCapabilities:
        """The explicit capabilities and degradation guarantees of this session."""
        return self._capabilities

    @property
    def mode(self) -> str:
        """Session mode ('proxy' or 'direct')."""
        return self._capabilities.mode

    @property
    def is_closed(self) -> bool:
        """Whether this session has been closed."""
        return self._closed

    def _check_not_closed(self) -> None:
        if self._closed:
            raise UnsupportedCapabilityError("HardwareSession is closed")

    async def mmio(self, resource: str) -> MmioRegion:
        """Acquires a named MMIO region. Fails closed in direct mode."""
        self._check_not_closed()
        self._capabilities.check_support("mmio")
        if self._proxy_session is None or self._proxy_description is None:
            raise UnsupportedCapabilityError("Proxy session is not available")
        info = self._proxy_description.resource_named(resource)
        if info is None:
            available = [r.name for r in self._proxy_description.resources]
            raise ValueError(
                f"Unknown MMIO resource '{resource}'. Available: {available}"
            )
        return MmioRegion(
            resource=info,
            session=self._proxy_session,
            audit_drainer=self._audit_drainer,
        )

    async def state(
        self, name: str = "state0", *, resource: str | None = None
    ) -> StateHandle:
        """Acquires a named StateBank resource handle (`StateHandle`). Fails closed in direct mode."""
        target_name = resource if resource is not None else name
        region = await self.mmio(target_name)
        return StateHandle(region)

    async def protocol(self, protocol_name: str) -> ProtocolProxy:
        """Connects to a published FIDL protocol. Fails in proxy mode."""
        self._check_not_closed()
        if self._direct_session is None:
            raise UnsupportedCapabilityError(
                "Direct published FIDL protocols are only supported in direct mode"
            )
        return ProtocolProxy(self._direct_session, protocol_name)

    async def gpio(self, resource: str = "gpio") -> Gpio:
        """Acquires a GPIO protocol adapter in proxy or direct mode."""
        self._check_not_closed()
        if (
            self._proxy_session is not None
            and self._proxy_description is not None
        ):
            info = self._proxy_description.resource_named(resource)
            if info is None:
                gpio_res = [
                    r
                    for r in self._proxy_description.resources
                    if r.kind == ResourceKind.GPIO
                ]
                if resource == "gpio" and gpio_res:
                    info = gpio_res[0]
                else:
                    available = [
                        r.name for r in self._proxy_description.resources
                    ]
                    raise ValueError(
                        f"Unknown GPIO resource '{resource}'. Available: {available}"
                    )
            return Gpio(
                self._proxy_session,
                resource_name=info.name,
                resource_info=info,
                audit_drainer=self._audit_drainer,
            )
        if self._direct_session is not None:
            return Gpio(self._direct_session, resource)
        raise UnsupportedCapabilityError(
            "GPIO protocol is not supported in current session"
        )

    async def i2c(self, resource: str = "i2c") -> I2c:
        """Acquires an I2C protocol adapter in proxy or direct mode."""
        self._check_not_closed()
        if (
            self._proxy_session is not None
            and self._proxy_description is not None
        ):
            info = self._proxy_description.resource_named(resource)
            if info is None:
                i2c_res = [
                    r
                    for r in self._proxy_description.resources
                    if r.kind == ResourceKind.I2C
                ]
                if resource == "i2c" and i2c_res:
                    info = i2c_res[0]
                else:
                    available = [
                        r.name for r in self._proxy_description.resources
                    ]
                    raise ValueError(
                        f"Unknown I2C resource '{resource}'. Available: {available}"
                    )
            return I2c(
                self._proxy_session,
                resource_name=info.name,
                resource_info=info,
                audit_drainer=self._audit_drainer,
            )
        if self._direct_session is not None:
            return I2c(self._direct_session, resource)
        raise UnsupportedCapabilityError(
            "I2C protocol is not supported in current session"
        )

    async def spi(self, resource: str = "spi") -> Spi:
        """Acquires a SPI protocol adapter in proxy or direct mode."""
        self._check_not_closed()
        if (
            self._proxy_session is not None
            and self._proxy_description is not None
        ):
            info = self._proxy_description.resource_named(resource)
            if info is None:
                spi_res = [
                    r
                    for r in self._proxy_description.resources
                    if r.kind == ResourceKind.SPI
                ]
                if resource == "spi" and spi_res:
                    info = spi_res[0]
                else:
                    available = [
                        r.name for r in self._proxy_description.resources
                    ]
                    raise ValueError(
                        f"Unknown SPI resource '{resource}'. Available: {available}"
                    )
            return Spi(
                self._proxy_session,
                resource_name=info.name,
                resource_info=info,
                audit_drainer=self._audit_drainer,
            )
        if self._direct_session is not None:
            return Spi(self._direct_session, resource)
        raise UnsupportedCapabilityError(
            "SPI protocol is not supported in current session"
        )

    async def serial(self, resource: str = "serial") -> Serial:
        """Acquires a Serial protocol adapter in direct mode."""
        self._check_not_closed()
        if self._direct_session is None:
            raise UnsupportedCapabilityError(
                "Serial protocol is only supported in direct mode"
            )
        return Serial(self._direct_session, resource)

    async def clock(self, resource: str = "clock") -> Clock:
        """Acquires a Clock protocol adapter in direct mode."""
        self._check_not_closed()
        if self._direct_session is None:
            raise UnsupportedCapabilityError(
                "Clock protocol is only supported in direct mode"
            )
        return Clock(self._direct_session, resource)

    async def reset(self, resource: str = "reset") -> Reset:
        """Acquires a Reset protocol adapter in direct mode."""
        self._check_not_closed()
        if self._direct_session is None:
            raise UnsupportedCapabilityError(
                "Reset protocol is only supported in direct mode"
            )
        return Reset(self._direct_session, resource)

    async def interrupt(self, resource: str = "interrupt") -> Interrupt:
        """Acquires an Interrupt adapter in proxy or direct mode (Spec Sections 11.7, 17)."""
        self._check_not_closed()
        if self._proxy_session is not None:
            if self._proxy_description is None:
                raise UnsupportedCapabilityError("Missing proxy description")
            info = None
            for r in self._proxy_description.resources:
                if (
                    r.name == resource or str(r.id) == resource
                ) and r.kind == ResourceKind.INTERRUPT:
                    info = r
                    break
            if info is None:
                irq_res = [
                    r
                    for r in self._proxy_description.resources
                    if r.kind == ResourceKind.INTERRUPT
                ]
                if resource == "interrupt" and irq_res:
                    info = irq_res[0]
                else:
                    available = [
                        r.name for r in self._proxy_description.resources
                    ]
                    raise ValueError(
                        f"Unknown interrupt resource '{resource}'. Available: {available}"
                    )
            return Interrupt(
                self._proxy_session,
                resource_name=info.name,
                resource_info=info,
                audit_drainer=self._audit_drainer,
            )
        if self._direct_session is not None:
            return Interrupt(self._direct_session, resource)
        raise UnsupportedCapabilityError(
            "Interrupt observation is not supported in current session"
        )

    async def sequence(
        self, operations: Sequence[SequenceItem]
    ) -> SequenceOutcome:
        """Executes a target-local sequence of operations (Spec Section 9)."""
        self._check_not_closed()
        self._capabilities.check_support("sequence")
        if self._proxy_session is None:
            raise UnsupportedCapabilityError("Proxy session is not available")
        outcome = await self._proxy_session.execute_sequence(tuple(operations))
        if self._audit_drainer is not None:
            await self._audit_drainer()
        return outcome

    async def close(self) -> None:
        """Closes the session, drains remaining audit, and releases target resources."""
        if self._closed:
            return
        self._closed = True
        try:
            if self._audit_drainer is not None:
                await self._audit_drainer()
        except (asyncio.CancelledError, Exception):
            pass
        finally:
            if self._proxy_session is not None:
                await self._proxy_session.close()
            if self._direct_session is not None:
                await self._direct_session.close()

    async def __aenter__(self) -> "HardwareSession":
        self._check_not_closed()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: Any,
    ) -> None:
        await self.close()


__all__ = [
    "AccessRequirements",
    "Clock",
    "Gpio",
    "HardwareSession",
    "I2c",
    "Interrupt",
    "MmioRegion",
    "PollResult",
    "ProtocolProxy",
    "Reset",
    "SequenceOutcome",
    "Serial",
    "SessionCapabilities",
    "Spi",
    "StateHandle",
    "TranslationMetadata",
    "UnsupportedCapabilityError",
    "WriteResult",
]
