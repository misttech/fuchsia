# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Protocol handlers for antlion tests of WLAN core.
"""

import logging

logger = logging.getLogger(__name__)


import asyncio
from dataclasses import dataclass
from typing import Any

import fidl_fuchsia_wlan_device_service as fidl_device_service
import fidl_fuchsia_wlan_sme as fidl_sme
from fuchsia_controller_py import Channel


@dataclass
class DeviceWatcherContext:
    txn_queue: asyncio.Queue[
        fidl_device_service.DeviceWatcherOnPhyAddedRequest
        | fidl_device_service.DeviceWatcherOnPhyRemovedRequest
        | fidl_device_service.DeviceWatcherOnIfaceAddedRequest
        | fidl_device_service.DeviceWatcherOnIfaceRemovedRequest
    ]


class DeviceWatcherEventHandler(fidl_device_service.DeviceWatcherEventHandler):
    def __init__(
        self,
        client: fidl_device_service.DeviceWatcherClient,
        verbose: bool = True,
    ) -> None:
        # Defer initialization of parent class to __aenter__
        self.client = client
        self.verbose = verbose

    def on_phy_added(
        self,
        request: fidl_device_service.DeviceWatcherOnPhyAddedRequest,
    ) -> None:
        if self.verbose:
            logger.info("phy added: %s", request)
        self.txn_queue.put_nowait(request)

    def on_phy_removed(
        self,
        request: fidl_device_service.DeviceWatcherOnPhyRemovedRequest,
    ) -> None:
        if self.verbose:
            logger.info("phy removed: %s", request)
        self.txn_queue.put_nowait(request)

    def on_iface_added(
        self,
        request: fidl_device_service.DeviceWatcherOnIfaceAddedRequest,
    ) -> None:
        if self.verbose:
            logger.info("iface added: %s", request)
        self.txn_queue.put_nowait(request)

    def on_iface_removed(
        self,
        request: fidl_device_service.DeviceWatcherOnIfaceRemovedRequest,
    ) -> None:
        if self.verbose:
            logger.info("iface removed: %s", request)
        self.txn_queue.put_nowait(request)

    async def __aenter__(self) -> DeviceWatcherContext:
        super().__init__(client=self.client)

        self.txn_queue: asyncio.Queue[
            fidl_device_service.DeviceWatcherOnPhyAddedRequest
            | fidl_device_service.DeviceWatcherOnPhyRemovedRequest
            | fidl_device_service.DeviceWatcherOnIfaceAddedRequest
            | fidl_device_service.DeviceWatcherOnIfaceRemovedRequest
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return DeviceWatcherContext(
            txn_queue=self.txn_queue,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task:
            self.server_task.cancel()


@dataclass
class ConnectTransactionContext:
    txn_queue: asyncio.Queue[
        fidl_sme.ConnectTransactionOnConnectResultRequest
        | fidl_sme.ConnectTransactionOnDisconnectRequest
        | fidl_sme.ConnectTransactionOnRoamResultRequest
        | fidl_sme.ConnectTransactionOnSignalReportRequest
        | fidl_sme.ConnectTransactionOnChannelSwitchedRequest
    ]
    server: Channel


class ConnectTransactionEventHandler(fidl_sme.ConnectTransactionEventHandler):
    def __init__(
        self,
        proxy: Channel,
        server: Channel,
        verbose: bool = True,
    ) -> None:
        self.proxy = proxy
        self.server = server
        # Defer initialization of parent class to __aenter__
        self.verbose = verbose

    def on_connect_result(
        self,
        request: fidl_sme.ConnectTransactionOnConnectResultRequest,
    ) -> None:
        if self.verbose:
            logger.info("Connect result: %s", request)
        self.txn_queue.put_nowait(request)

    def on_disconnect(
        self,
        request: fidl_sme.ConnectTransactionOnDisconnectRequest,
    ) -> None:
        if self.verbose:
            logger.info("Disconnect: %s", request)
        self.txn_queue.put_nowait(request)

    def on_roam_result(
        self,
        request: fidl_sme.ConnectTransactionOnRoamResultRequest,
    ) -> None:
        if self.verbose:
            logger.info("Roam result: %s", request)
        self.txn_queue.put_nowait(request)

    def on_signal_report(
        self,
        request: fidl_sme.ConnectTransactionOnSignalReportRequest,
    ) -> None:
        if self.verbose:
            logger.info("Signal report: %s", request)
        self.txn_queue.put_nowait(request)

    def on_channel_switched(
        self,
        request: fidl_sme.ConnectTransactionOnChannelSwitchedRequest,
    ) -> None:
        if self.verbose:
            logger.info("Channel switched: %s", request)
        self.txn_queue.put_nowait(request)

    async def __aenter__(self) -> ConnectTransactionContext:
        super().__init__(
            client=fidl_sme.ConnectTransactionClient(self.proxy.take())
        )
        self.txn_queue: asyncio.Queue[
            fidl_sme.ConnectTransactionOnConnectResultRequest
            | fidl_sme.ConnectTransactionOnDisconnectRequest
            | fidl_sme.ConnectTransactionOnRoamResultRequest
            | fidl_sme.ConnectTransactionOnSignalReportRequest
            | fidl_sme.ConnectTransactionOnChannelSwitchedRequest
        ] = asyncio.Queue()
        self.server_task = asyncio.create_task(self.serve())
        return ConnectTransactionContext(
            txn_queue=self.txn_queue,
            server=self.server,
        )

    async def __aexit__(self, *args: Any, **kwargs: Any) -> None:
        if self.server_task:
            self.server_task.cancel()
