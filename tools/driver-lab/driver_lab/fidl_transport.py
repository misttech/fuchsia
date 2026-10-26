# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""ProxyTransport implementation over the `fuchsia.driver.lab` FIDL
bindings via fuchsia-controller.

The adapter is connection-agnostic: it takes an already-connected
`Proxy` client and a channel factory, so the same code serves
host-side round-trip tests (in-process channel pairs) and a real target
(channels from a device-connected fuchsia-controller context).
"""

from collections.abc import Callable, Sequence
from typing import Any

import fidl_fuchsia_driver_lab as fdl
from driver_lab.models import (
    AccessClass,
    GpioReadOutcome,
    GpioWriteOutcome,
    I2cTransferOutcome,
    InterruptOutcome,
    ResourceKind,
    SpiTransmitOutcome,
)
from driver_lab.transport import (
    AllowRule,
    AuditEntry,
    AuditPage,
    Denial,
    Expectations,
    OpenRejection,
    OpenSessionRejected,
    OperationDenied,
    PollOutcome,
    ProxyDescription,
    ProxySession,
    ReadOutcome,
    ResourceInfo,
    SequenceItem,
    SequenceItemOutcome,
    SequenceOutcome,
    SessionContext,
    SessionMode,
    SnapshotItem,
    SnapshotItemOutcome,
    SnapshotOutcome,
    TransportError,
    WriteOutcome,
)

_ACCESS_TO_FIDL = {
    AccessClass.READ_ONCE: fdl.AccessClass.READ_ONCE,
    AccessClass.SNAPSHOT: fdl.AccessClass.SNAPSHOT,
    AccessClass.POLL: fdl.AccessClass.POLL,
    AccessClass.WRITE: fdl.AccessClass.WRITE,
    AccessClass.SEQUENCE: fdl.AccessClass.SEQUENCE,
    AccessClass.PROTOCOL: fdl.AccessClass.PROTOCOL,
    AccessClass.INTERRUPT: fdl.AccessClass.INTERRUPT,
}

_RESOURCE_KIND_FROM_FIDL = {
    fdl.ResourceKind.MMIO: ResourceKind.MMIO,
    fdl.ResourceKind.GPIO: ResourceKind.GPIO,
    fdl.ResourceKind.I2_C: ResourceKind.I2C,
    fdl.ResourceKind.SPI: ResourceKind.SPI,
    fdl.ResourceKind.INTERRUPT: ResourceKind.INTERRUPT,
    fdl.ResourceKind.CLOCK: ResourceKind.CLOCK,
    fdl.ResourceKind.RESET: ResourceKind.RESET,
    fdl.ResourceKind.SERIAL: ResourceKind.SERIAL,
}


def _to_resource_kind(val: Any) -> ResourceKind:
    if val is None:
        return ResourceKind.MMIO
    if isinstance(val, int):
        val = fdl.ResourceKind(val)
    return _RESOURCE_KIND_FROM_FIDL.get(val, ResourceKind.MMIO)


def _unwrap(result: Any) -> Any:
    """Unwraps a flexible-method result wrapper when one is present."""
    unwrap = getattr(result, "unwrap", None)
    if callable(unwrap):
        return unwrap()
    return result


def _enum_lower(value: Any) -> str | None:
    if value is None:
        return None
    name = getattr(value, "name", None)
    if name is None:
        return str(value)
    return str(name).lower()


def _enum_name(enum_type: Any, value: Any) -> str | None:
    """Decoded enum fields may arrive as raw ints; rehydrate before naming."""
    if value is None:
        return None
    if not hasattr(value, "name"):
        value = enum_type(value)
    return _enum_lower(value)


def _to_denial(error: Any) -> Denial:
    if isinstance(error, int):
        error = fdl.OperationError(error)
    name = _enum_lower(error)
    try:
        return Denial(name)
    except ValueError as exc:
        raise TransportError(
            f"unknown operation error from target: {error!r}"
        ) from exc


def _to_rejection(error: Any) -> OpenRejection:
    if isinstance(error, int):
        error = fdl.OpenSessionError(error)
    name = _enum_lower(error)
    try:
        return OpenRejection(name)
    except ValueError as exc:
        raise TransportError(
            f"unknown open-session error from target: {error!r}"
        ) from exc


def _check_denied(result: Any) -> None:
    error = getattr(result, "err", None)
    if error is not None:
        raise OperationDenied(_to_denial(error))


def connect_transport(
    moniker: str,
    capability: str = "fuchsia.driver.lab.Service/default/proxy",
    target: str | None = None,
    config: dict[str, str] | None = None,
) -> "FidlProxyTransport":
    """Connects to the proxy served by a target device.

    Establishes a fuchsia-controller `Context` (the same transport ffx
    uses), connects to `capability` at the driver component `moniker`,
    and returns the adapter. The adapter itself is exercised through
    real FIDL encoding in host tests; this device connection path is
    validated by the emulator conformance suite.
    """
    from fuchsia_controller_py import Context

    context = Context(config=config, target=target)
    channel = context.connect_device_proxy(moniker, capability)
    return FidlProxyTransport(fdl.ProxyClient(channel), context.channel_create)


class LazyFidlProxyTransport:
    """Connects to the proxy on first use (e.g. after bind_proxy activates it)."""

    def __init__(
        self,
        moniker: str,
        capability: str = "fuchsia.driver.lab.Service/default/proxy",
        target: str | None = None,
        config: dict[str, str] | None = None,
    ) -> None:
        self._moniker = moniker
        self._capability = capability
        self._target = target
        self._config = config
        self._inner: FidlProxyTransport | None = None

    def _get_inner(self) -> FidlProxyTransport:
        if self._inner is None:
            self._inner = connect_transport(
                moniker=self._moniker,
                capability=self._capability,
                target=self._target,
                config=self._config,
            )
        return self._inner

    async def describe(self) -> ProxyDescription:
        return await self._get_inner().describe()

    async def open_session(
        self,
        context: SessionContext,
        expectations: Expectations,
        allowlist: Sequence[AllowRule],
        mode: SessionMode = SessionMode.READ_ONLY,
    ) -> ProxySession:
        return await self._get_inner().open_session(
            context=context,
            expectations=expectations,
            allowlist=allowlist,
            mode=mode,
        )


class FidlProxyTransport:
    """`ProxyTransport` over generated `fuchsia.driver.lab` bindings."""

    def __init__(
        self,
        proxy: Any,
        channel_factory: Callable[[], tuple[Any, Any]],
    ) -> None:
        """`proxy` is a connected `ProxyClient`; `channel_factory`
        returns a (client, server) channel pair in the same handle domain
        as the proxy connection."""
        self._probe = proxy
        self._channel_factory = channel_factory

    async def describe(self) -> ProxyDescription:
        """See `ProxyTransport.describe`."""
        description = _unwrap(await self._probe.describe())
        resources = tuple(
            ResourceInfo(
                id=resource.id_ or 0,
                name=resource.name or "",
                kind=_to_resource_kind(resource.kind),
                logical_size=resource.logical_size or 0,
                digest=resource.digest or "",
            )
            for resource in (description.resources or [])
        )
        return ProxyDescription(
            protocol_major=description.protocol_major or 0,
            protocol_minor=description.protocol_minor or 0,
            proxy_generation=description.proxy_generation or 0,
            boot_id=description.boot_id or "",
            resource_digest=description.resource_digest or "",
            policy_digest=description.policy_digest or "",
            resources=resources,
            max_snapshot_items=description.max_snapshot_items or 0,
            audit_capacity=description.audit_capacity or 0,
            node_moniker=description.node_moniker,
            topology_generation=description.topology_generation,
            takeover=_enum_name(fdl.TakeoverState, description.takeover),
        )

    async def open_session(
        self,
        context: SessionContext,
        expectations: Expectations,
        allowlist: Sequence[AllowRule],
        mode: SessionMode = SessionMode.READ_ONLY,
    ) -> "_FidlProxySession":
        """See `ProxyTransport.open_session`."""
        client_channel, server_channel = self._channel_factory()
        fidl_mode = (
            fdl.SessionMode.MUTATING
            if mode == SessionMode.MUTATING
            else fdl.SessionMode.READ_ONLY
        )
        result = await self._probe.open_session(
            context=fdl.RunContext(
                run_id=context.run_id,
                case_id=context.case_id,
                plan_digest=context.plan_digest,
                host_tool_version=context.host_tool_version,
            ),
            mode=fidl_mode,
            expectations=fdl.Expectations(
                boot_id=expectations.boot_id,
                proxy_generation=expectations.proxy_generation,
                resource_digest=expectations.resource_digest,
                policy_digest=expectations.policy_digest,
            ),
            allowlist=[
                fdl.AccessRule(
                    resource=rule.resource,
                    offset=rule.offset,
                    width=rule.width,
                    class_=_ACCESS_TO_FIDL[rule.access],
                )
                for rule in allowlist
            ],
            session=server_channel.take(),
        )
        error = getattr(result, "err", None)
        if error is not None:
            raise OpenSessionRejected(_to_rejection(error))
        return _FidlProxySession(fdl.SessionClient(client_channel))


class _FidlProxySession:
    """`ProxySession` over a `Session` client channel."""

    def __init__(self, session: Any) -> None:
        self._session = session

    async def read32(self, resource: int, offset: int) -> ReadOutcome:
        """See `ProxySession.read32`."""
        result = await self._session.read32(resource=resource, offset=offset)
        _check_denied(result)
        response = _unwrap(result)
        return ReadOutcome(
            value=response.value,
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def snapshot(self, items: Sequence[SnapshotItem]) -> SnapshotOutcome:
        """See `ProxySession.snapshot`."""
        result = await self._session.snapshot(
            items=[
                fdl.SnapshotItem(resource=item.resource, offset=item.offset)
                for item in items
            ]
        )
        _check_denied(result)
        response = _unwrap(result)
        return SnapshotOutcome(
            results=tuple(
                SnapshotItemOutcome(
                    item=SnapshotItem(
                        resource=entry.item.resource, offset=entry.item.offset
                    ),
                    ok=entry.ok,
                    value=entry.value,
                    audit_seq=entry.audit_seq,
                    timestamp_ns=entry.timestamp_ns,
                )
                for entry in response.results
            ),
            complete=response.complete,
        )

    async def write32(
        self,
        resource: int,
        offset: int,
        value: int,
        write_mask: int = 0xFFFF_FFFF,
        precondition: tuple[int, int] | None = None,
        readback: bool = True,
    ) -> WriteOutcome:
        """See `ProxySession.write32`."""
        precond = None
        if precondition is not None:
            precond = fdl.WritePrecondition(
                expected=precondition[0], mask=precondition[1]
            )
        result = await self._session.write32(
            resource=resource,
            offset=offset,
            value=value,
            write_mask=write_mask,
            precondition=precond,
            readback=readback,
        )
        _check_denied(result)
        response = _unwrap(result)
        rb_val = response.readback_value if readback else None
        return WriteOutcome(
            readback_value=rb_val,
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def poll32(
        self,
        resource: int,
        offset: int,
        expected: int,
        mask: int = 0xFFFF_FFFF,
        interval_ns: int = 1_000_000,
        timeout_ns: int = 100_000_000,
    ) -> PollOutcome:
        """See `ProxySession.poll32`."""
        result = await self._session.poll32(
            resource=resource,
            offset=offset,
            expected=expected,
            mask=mask,
            interval_ns=interval_ns,
            timeout_ns=timeout_ns,
        )
        _check_denied(result)
        response = _unwrap(result)
        return PollOutcome(
            value=response.value,
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def gpio_read(self, resource: int) -> GpioReadOutcome:
        """See `ProxySession.gpio_read`."""
        result = await self._session.gpio_read(resource=resource)
        _check_denied(result)
        response = _unwrap(result)
        return GpioReadOutcome(
            value=response.value,
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def gpio_write(self, resource: int, value: bool) -> GpioWriteOutcome:
        """See `ProxySession.gpio_write`."""
        result = await self._session.gpio_write(resource=resource, value=value)
        _check_denied(result)
        response = _unwrap(result)
        return GpioWriteOutcome(
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def i2c_transfer(
        self, resource: int, write_data: bytes = b"", read_length: int = 0
    ) -> I2cTransferOutcome:
        """See `ProxySession.i2c_transfer`."""
        result = await self._session.i2c_transfer(
            resource=resource,
            write_data=list(write_data),
            read_length=read_length,
        )
        _check_denied(result)
        response = _unwrap(result)
        return I2cTransferOutcome(
            read_data=bytes(response.read_data),
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def spi_transmit(
        self, resource: int, tx_data: bytes
    ) -> SpiTransmitOutcome:
        """See `ProxySession.spi_transmit`."""
        result = await self._session.spi_transmit(
            resource=resource,
            tx_data=list(tx_data),
        )
        _check_denied(result)
        response = _unwrap(result)
        return SpiTransmitOutcome(
            rx_data=bytes(response.rx_data),
            audit_seq=response.audit_seq,
            timestamp_ns=response.timestamp_ns,
        )

    async def execute_sequence(
        self, items: Sequence[SequenceItem]
    ) -> SequenceOutcome:
        """See `ProxySession.execute_sequence`."""
        fidl_items: list[fdl.SequenceItem] = []
        for item in items:
            if item.kind in ("mmio_read32", "state_read32", "read32"):
                fidl_items.append(
                    fdl.SequenceItem(
                        read32=fdl.Read32(
                            resource=item.resource, offset=item.offset
                        )
                    )
                )
            elif item.kind in (
                "mmio_write32",
                "knob_write32",
                "trigger_write32",
                "write32",
            ):
                precond = None
                if item.precondition is not None:
                    precond = fdl.WritePrecondition(
                        expected=item.precondition[0],
                        mask=item.precondition[1],
                    )
                fidl_items.append(
                    fdl.SequenceItem(
                        write32=fdl.Write32(
                            resource=item.resource,
                            offset=item.offset,
                            value=item.value,
                            write_mask=item.write_mask,
                            precondition=precond,
                            readback=item.readback,
                        )
                    )
                )
            elif item.kind in ("mmio_poll32", "state_poll32", "poll32"):
                fidl_items.append(
                    fdl.SequenceItem(
                        poll32=fdl.Poll32(
                            resource=item.resource,
                            offset=item.offset,
                            expected=item.expected,
                            mask=item.mask,
                            interval_ns=item.interval_ns,
                            timeout_ns=item.timeout_ns,
                        )
                    )
                )
            elif item.kind == "delay_ns":
                fidl_items.append(fdl.SequenceItem(delay_ns=item.delay_ns))
            elif item.kind == "barrier":
                fidl_items.append(
                    fdl.SequenceItem(barrier=fdl.BarrierVariant.MEMORY)
                )
            elif item.kind == "gpio_read":
                fidl_items.append(
                    fdl.SequenceItem(
                        gpio_read=fdl.GpioRead(resource=item.resource)
                    )
                )
            elif item.kind == "gpio_write":
                fidl_items.append(
                    fdl.SequenceItem(
                        gpio_write=fdl.GpioWrite(
                            resource=item.resource, value=bool(item.value)
                        )
                    )
                )
            elif item.kind == "i2c_transfer":
                fidl_items.append(
                    fdl.SequenceItem(
                        i2c_transfer=fdl.I2cTransfer(
                            resource=item.resource,
                            write_data=list(item.write_data),
                            read_length=item.read_length,
                        )
                    )
                )
            elif item.kind == "spi_transmit":
                fidl_items.append(
                    fdl.SequenceItem(
                        spi_transmit=fdl.SpiTransmit(
                            resource=item.resource,
                            tx_data=list(item.tx_data),
                        )
                    )
                )
        result = await self._session.execute_sequence(items=fidl_items)
        _check_denied(result)
        response = _unwrap(result)
        outcomes: list[SequenceItemOutcome] = []
        for item_res in response.results:
            outcome = item_res.outcome
            kind = "unknown"
            val = None
            rb = None
            data = b""
            audit_seq = 0
            ts_ns = 0
            err = None
            if outcome.read32:
                kind = "read32"
                val = outcome.read32.value
                audit_seq = outcome.read32.audit_seq
                ts_ns = outcome.read32.timestamp_ns
            elif outcome.write32:
                kind = "write32"
                rb = outcome.write32.readback_value
                audit_seq = outcome.write32.audit_seq
                ts_ns = outcome.write32.timestamp_ns
            elif outcome.poll32:
                kind = "poll32"
                val = outcome.poll32.value
                audit_seq = outcome.poll32.audit_seq
                ts_ns = outcome.poll32.timestamp_ns
            elif outcome.delay_ns:
                kind = "delay_ns"
            elif outcome.barrier:
                kind = "barrier"
            elif outcome.gpio_read:
                kind = "gpio_read"
                val = 1 if outcome.gpio_read.value else 0
                audit_seq = outcome.gpio_read.audit_seq
                ts_ns = outcome.gpio_read.timestamp_ns
            elif outcome.gpio_write:
                kind = "gpio_write"
                audit_seq = outcome.gpio_write.audit_seq
                ts_ns = outcome.gpio_write.timestamp_ns
            elif outcome.i2c_transfer:
                kind = "i2c_transfer"
                data = bytes(outcome.i2c_transfer.read_data)
                audit_seq = outcome.i2c_transfer.audit_seq
                ts_ns = outcome.i2c_transfer.timestamp_ns
            elif outcome.spi_transmit:
                kind = "spi_transmit"
                data = bytes(outcome.spi_transmit.rx_data)
                audit_seq = outcome.spi_transmit.audit_seq
                ts_ns = outcome.spi_transmit.timestamp_ns
            elif outcome.error:
                kind = "error"
                err = _to_denial(outcome.error)
            outcomes.append(
                SequenceItemOutcome(
                    index=item_res.index,
                    ok=item_res.ok,
                    kind=kind,
                    value=val,
                    readback_value=rb,
                    data=data,
                    audit_seq=audit_seq,
                    timestamp_ns=ts_ns,
                    error=err,
                )
            )
        return SequenceOutcome(
            results=tuple(outcomes), complete=response.complete
        )

    async def read_audit(self, cursor: int, limit: int) -> AuditPage:
        """See `ProxySession.read_audit`."""
        response = _unwrap(
            await self._session.read_audit(cursor=cursor, limit=limit)
        )
        entries = tuple(
            AuditEntry(
                seq=entry.seq or 0,
                operation=entry.operation or "",
                session=entry.session,
                resource=entry.resource,
                offset=entry.offset,
                decision=_enum_name(fdl.AuditDecision, entry.decision)
                or "allowed",
                denial=_enum_name(fdl.OperationError, entry.denial),
                status=_enum_name(fdl.AuditStatus, entry.status) or "ok",
                value=entry.value,
                timestamp_ns=entry.timestamp_ns or 0,
                run_id=entry.run_id,
                item_index=entry.item_index,
                proxy_generation=entry.proxy_generation,
                boot_id=entry.boot_id,
            )
            for entry in response.entries
        )
        oldest = response.oldest_retained if response.has_oldest else None
        return AuditPage(
            entries=entries,
            oldest_retained=oldest,
            next_cursor=response.next_cursor,
        )

    async def wait_for_interrupt(
        self,
        resource: int,
        after_sequence: int = 0,
        timeout_s: float = 1.0,
    ) -> InterruptOutcome:
        """See `ProxySession.wait_for_interrupt`."""
        timeout_ns = int(timeout_s * 1_000_000_000)
        result = await self._session.wait_for_interrupt(
            resource=resource,
            after_sequence=after_sequence,
            timeout_ns=timeout_ns,
        )
        _check_denied(result)
        response = _unwrap(result)
        return InterruptOutcome(
            resource=response.resource,
            sequence=response.sequence,
            count=response.count,
            timestamp_ns=response.timestamp_ns,
            coalesced_count=response.coalesced_count,
        )

    async def close(self) -> None:
        """See `ProxySession.close`. Dropping the client closes the channel."""
        self._session = None
