# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Reusable FIDL callback and event servers for wlanix tests."""

import asyncio
import logging
from dataclasses import dataclass
from typing import Any

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
from fuchsia_controller_py import Channel

logger = logging.getLogger(__name__)


@dataclass
class WifiEventCallbackContext:
    subsystem_restart_received: asyncio.Event
    start_received: asyncio.Event
    stop_received: asyncio.Event
    callback_channel: Channel


class WifiEventCallbackServer(fidl_wlanix.WifiEventCallbackServer):
    """Server for fuchsia.wlan.wlanix/WifiEventCallback."""

    def __init__(self, client: Channel, server: Channel) -> None:
        # Defer initialization of parent class to __aenter__
        self.client = client
        self.server = server
        self.subsystem_restart_received = asyncio.Event()
        self.start_received = asyncio.Event()
        self.stop_received = asyncio.Event()
        self.server_task: asyncio.Task[None] | None = None

    def on_subsystem_restart(
        self, payload: fidl_wlanix.WifiEventCallbackOnSubsystemRestartRequest
    ) -> None:
        logger.info("Received OnSubsystemRestart: payload=%s", payload)
        self.subsystem_restart_received.set()

    def on_start(self) -> None:
        self.start_received.set()

    def on_stop(self) -> None:
        self.stop_received.set()

    async def __aenter__(self) -> WifiEventCallbackContext:
        super().__init__(channel=self.server)
        self.server_task = asyncio.create_task(self.serve())
        return WifiEventCallbackContext(
            subsystem_restart_received=self.subsystem_restart_received,
            start_received=self.start_received,
            stop_received=self.stop_received,
            callback_channel=self.client,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task is not None:
            self.server_task.cancel()
            try:
                await self.server_task
            except asyncio.CancelledError:
                pass


@dataclass
class SupplicantStaIfaceCallbackContext:
    state_change_queue: asyncio.Queue[
        fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest
        | fidl_wlanix.SupplicantStaIfaceCallbackOnDisconnectedRequest
        | fidl_wlanix.SupplicantStaIfaceCallbackOnAssociationRejectedRequest
    ]
    callback_channel: Channel


class SupplicantStaIfaceCallbackServer(
    fidl_wlanix.SupplicantStaIfaceCallbackServer
):
    """Server for fuchsia.wlan.wlanix/SupplicantStaIfaceCallback."""

    def __init__(
        self,
        client: Channel,
        server: Channel,
        verbose: bool = True,
    ) -> None:
        # Defer initialization of parent class to __aenter__
        self.client = client
        self.server = server
        self.verbose = verbose
        self.server_task: asyncio.Task[None] | None = None

    def on_state_changed(
        self,
        request: fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest,
    ) -> None:
        if self.verbose:
            logger.info("State changed: %s", request)
        self.state_change_queue.put_nowait(request)

    def on_disconnected(
        self,
        request: fidl_wlanix.SupplicantStaIfaceCallbackOnDisconnectedRequest,
    ) -> None:
        if self.verbose:
            logger.info("Disconnected: %s", request)
        self.state_change_queue.put_nowait(request)

    def on_association_rejected(
        self,
        request: fidl_wlanix.SupplicantStaIfaceCallbackOnAssociationRejectedRequest,
    ) -> None:
        if self.verbose:
            logger.info("Association rejected: %s", request)
        self.state_change_queue.put_nowait(request)

    async def __aenter__(self) -> SupplicantStaIfaceCallbackContext:
        super().__init__(channel=self.server)

        self.state_change_queue: asyncio.Queue[
            fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest
            | fidl_wlanix.SupplicantStaIfaceCallbackOnDisconnectedRequest
            | fidl_wlanix.SupplicantStaIfaceCallbackOnAssociationRejectedRequest
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return SupplicantStaIfaceCallbackContext(
            state_change_queue=self.state_change_queue,
            callback_channel=self.client,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task is not None:
            self.server_task.cancel()
            try:
                await self.server_task
            except asyncio.CancelledError:
                pass


@dataclass
class Nl80211MulticastServerContext:
    message_queue: asyncio.Queue[fidl_wlanix.Nl80211Message]
    callback_channel: Channel


class Nl80211MulticastServer(fidl_wlanix.Nl80211MulticastServer):
    """Server for fuchsia.wlan.wlanix/Nl80211Multicast."""

    def __init__(self, client: Channel, server: Channel) -> None:
        # Defer initialization of parent class to __aenter__
        self.client = client
        self.server = server
        self.server_task: asyncio.Task[None] | None = None

    async def message(
        self, request: fidl_wlanix.Nl80211MulticastMessageRequest
    ) -> None:
        if request.message is not None:
            self.message_queue.put_nowait(request.message)

    async def __aenter__(self) -> Nl80211MulticastServerContext:
        super().__init__(channel=self.server)
        self.message_queue: asyncio.Queue[
            fidl_wlanix.Nl80211Message
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return Nl80211MulticastServerContext(
            message_queue=self.message_queue,
            callback_channel=self.client,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task is not None:
            self.server_task.cancel()
            try:
                await self.server_task
            except asyncio.CancelledError:
                pass
