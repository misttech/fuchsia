# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit and round-trip FIDL tests for driver node discovery."""

import asyncio
import os
import unittest
from typing import Any

import fidl_fuchsia_driver_development as fdd
from driver_lab.discovery import (
    FakeNodeDiscovery,
    FidlNodeDiscovery,
    NodeDescription,
)
from fidl._ipc import GlobalHandleWaker
from fuchsia_controller_py import Channel, Context


class _BridgeIteratorServer(fdd.NodeInfoIteratorServer):
    """Serves one NodeInfoIterator channel by returning batches."""

    def __init__(self, channel: Channel, items: list[Any]) -> None:
        super().__init__(channel)
        self._items = items
        self._sent = False

    async def get_next(self) -> Any:
        if not self._sent:
            self._sent = True
            return fdd.NodeInfoIteratorGetNextResponse(nodes=self._items)
        return fdd.NodeInfoIteratorGetNextResponse(nodes=[])


class _BridgeManagerServer(fdd.ManagerServer):
    """Serves Manager protocol over an in-process channel."""

    def __init__(
        self,
        channel: Channel,
        nodes: list[Any],
        tasks: list[asyncio.Task[None]],
    ) -> None:
        super().__init__(channel)
        self._nodes = nodes
        self._tasks = tasks

    async def get_node_info(self, request: Any) -> Any:
        node_filter = request.node_filter or []
        exact_match = request.exact_match

        matching = []
        for n in self._nodes:
            m = n.moniker or ""
            if not node_filter:
                matching.append(n)
            elif exact_match:
                if any(m == f for f in node_filter):
                    matching.append(n)
            else:
                if any(f in m for f in node_filter):
                    matching.append(n)

        iterator_channel = request.iterator
        if not isinstance(iterator_channel, Channel):
            iterator_channel = Channel(iterator_channel)
        iterator_server = _BridgeIteratorServer(iterator_channel, matching)
        self._tasks.append(
            asyncio.get_running_loop().create_task(iterator_server.serve())
        )
        return None

    async def get_driver_info(self, request: Any) -> Any:
        return None

    async def get_composite_node_specs(self, request: Any) -> Any:
        return None

    async def get_composite_info(self, request: Any) -> Any:
        return None

    async def get_driver_host_info(self, request: Any) -> Any:
        return None

    async def restart_driver_hosts(self, request: Any) -> Any:
        return None

    async def disable_driver(self, request: Any) -> Any:
        return None

    async def enable_driver(self, request: Any) -> Any:
        return None

    async def bind_all_unbound_nodes(self) -> Any:
        return None

    async def bind_all_unbound_nodes2(self) -> Any:
        return None

    async def add_test_node(self, request: Any) -> Any:
        return None

    async def remove_test_node(self, request: Any) -> Any:
        return None

    async def wait_for_bootup(self) -> Any:
        return None

    async def restart_with_dictionary(self, request: Any) -> Any:
        return None

    async def restart_with_dictionary_and_power_dependencies(
        self, request: Any
    ) -> Any:
        return None

    async def rebind_composites_with_driver(self, request: Any) -> Any:
        return None

    async def run_health_check(self, request: Any) -> Any:
        return None


class FakeDiscoveryTest(unittest.IsolatedAsyncioTestCase):
    async def test_fake_discovery_listing_and_filtering(self) -> None:
        disc = FakeNodeDiscovery(
            [
                NodeDescription(
                    moniker="root.dev.pci-00",
                    bound_driver_url="fuchsia-boot:///pci#meta/pci.cm",
                    driver_host_koid=1234,
                    properties={"name": "pci"},
                ),
                NodeDescription(
                    moniker="root.dev.pci-00.test-node",
                    bound_driver_url=None,
                    properties={"fuchsia.driver.lab.PROXY_TARGET": "selected"},
                ),
            ]
        )

        all_nodes = await disc.list_nodes()
        self.assertEqual(len(all_nodes), 2)

        unclaimed = await disc.find_unclaimed()
        self.assertEqual(len(unclaimed), 1)
        self.assertEqual(unclaimed[0].moniker, "root.dev.pci-00.test-node")
        self.assertTrue(unclaimed[0].is_unclaimed)

        filtered = await disc.list_nodes(node_filter=["test-node"])
        self.assertEqual(len(filtered), 1)
        self.assertEqual(filtered[0].moniker, "root.dev.pci-00.test-node")

        exact = await disc.list_nodes(
            node_filter=["root.dev.pci-00"], exact_match=True
        )
        self.assertEqual(len(exact), 1)
        self.assertFalse(exact[0].is_unclaimed)
        self.assertEqual(exact[0].driver_host_koid, 1234)

        desc = await disc.describe_node("root.dev.pci-00.test-node")
        self.assertIsNotNone(desc)
        assert desc is not None
        self.assertTrue(desc.is_unclaimed)
        self.assertEqual(
            desc.properties["fuchsia.driver.lab.PROXY_TARGET"], "selected"
        )

        missing = await disc.describe_node("non-existent")
        self.assertIsNone(missing)


class FidlDiscoveryTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        GlobalHandleWaker()._reset_for_testing()
        self.tasks: list[asyncio.Task[None]] = []
        self._orig_nodename = os.environ.pop("FUCHSIA_NODENAME", None)
        self._orig_device_addr = os.environ.pop("FUCHSIA_DEVICE_ADDR", None)

    async def asyncTearDown(self) -> None:
        for task in self.tasks:
            task.cancel()
        if self._orig_nodename is not None:
            os.environ["FUCHSIA_NODENAME"] = self._orig_nodename
        if self._orig_device_addr is not None:
            os.environ["FUCHSIA_DEVICE_ADDR"] = self._orig_device_addr

    async def test_fidl_round_trip_discovery(self) -> None:
        context = Context(target="")
        client_chan, server_chan = context.channel_create()

        test_nodes = [
            fdd.NodeInfo(
                id_=1,
                moniker="dev.sys.platform",
                bound_driver_url="fuchsia-boot:///platform-bus#meta/platform-bus.cm",
                driver_host_koid=4096,
                quarantined=False,
            ),
            fdd.NodeInfo(
                id_=2,
                moniker="dev.sys.platform.unclaimed-sensor",
                bound_driver_url="",
                driver_host_koid=None,
                quarantined=False,
            ),
        ]

        server = _BridgeManagerServer(server_chan, test_nodes, self.tasks)
        self.tasks.append(
            asyncio.get_running_loop().create_task(server.serve())
        )

        discovery = FidlNodeDiscovery(
            fdd.ManagerClient(client_chan),
            context.channel_create,
        )

        all_nodes = await discovery.list_nodes()
        self.assertEqual(len(all_nodes), 2)

        unclaimed = await discovery.find_unclaimed()
        self.assertEqual(len(unclaimed), 1)
        self.assertEqual(
            unclaimed[0].moniker, "dev.sys.platform.unclaimed-sensor"
        )
        self.assertTrue(unclaimed[0].is_unclaimed)

        described = await discovery.describe_node(
            "dev.sys.platform.unclaimed-sensor"
        )
        self.assertIsNotNone(described)
        assert described is not None
        self.assertEqual(described.moniker, "dev.sys.platform.unclaimed-sensor")
        self.assertTrue(described.is_unclaimed)

        missing = await discovery.describe_node("missing-node")
        self.assertIsNone(missing)


if __name__ == "__main__":
    unittest.main()
