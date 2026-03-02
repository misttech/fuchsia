# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Module for handling encoding and decoding FIDL messages, as well as for handling async I/O."""

import asyncio
import logging
import socket
import traceback
from abc import ABC, abstractmethod
from typing import Dict

import fuchsia_controller_py as fc

logger = logging.getLogger(__name__)


HANDLE_READY_QUEUES: Dict[int, asyncio.Queue[int]] = {}


def enqueue_ready_zx_handle_from_fd(
    fd: int, handle_ready_queues: Dict[int, asyncio.Queue[int]]
) -> None:
    """Reads zx_handle that is ready for reading, and enqueues it in the appropriate ready queue."""
    s = socket.fromfd(fd, socket.AF_UNIX, socket.SOCK_STREAM)
    handle_no = int.from_bytes(s.recv(4), "little")

    queue = handle_ready_queues.get(handle_no)
    if not queue:
        logger.debug(f"Dropping notification for: {handle_no}")
        return

    queue.put_nowait(handle_no)


class HandleWaker(ABC):
    """Base class for a waker used with potentially blocking handles."""

    @abstractmethod
    def register(self, channel: fc.BaseHandle, *, name: str) -> None:
        """Registers a handle to receive wake notifications."""

    @abstractmethod
    def unregister(self, channel: fc.BaseHandle) -> None:
        """Unregisters a handle, meaning it is not possible to wait for it to be ready."""

    @abstractmethod
    def post_ready(self, channel: fc.BaseHandle) -> None:
        """Notifies the waker that a channel is ready."""

    @abstractmethod
    async def wait_ready(self, channel: fc.BaseHandle) -> int:
        """Waits for a channel to be ready asynchronously."""


class GlobalHandleWaker(HandleWaker):
    """A class for handling notifications on a readable handle.

    As this is a global channel waker, this hooks into global state. This is the default waker used
    with all client and server code, as well as async wrapper code around readable handles.
    """

    def __init__(self) -> None:
        self.handle_ready_queues = HANDLE_READY_QUEUES

    def register(self, h: fc.BaseHandle, *, name: str) -> None:
        if h.as_int() not in self.handle_ready_queues:
            self.handle_ready_queues[h.as_int()] = asyncio.Queue()
        notification_fd = fc.connect_handle_notifier()
        # This try call is simply here in case registration occurs in outside
        # an async event loop.
        try:
            # Calling this multiple times only overwrites the reader.
            # In the event that the loop is destroyed this will be removed automatically.
            asyncio.get_running_loop().add_reader(
                notification_fd,
                enqueue_ready_zx_handle_from_fd,
                notification_fd,
                self.handle_ready_queues,
            )
        except RuntimeError as e:
            logger.debug(
                "Registered reader in non-async context. Printing exception for debugging."
            )
            logger.debug("[[ TRACE BEGIN ]]")
            for line in map(lambda x: x.strip(), traceback.format_exception(e)):
                logger.debug(f"-- {line}")
            logger.debug("[[ TRACE END ]]")

    def unregister(self, h: fc.BaseHandle) -> None:
        if h.as_int() in self.handle_ready_queues:
            self.handle_ready_queues.pop(h.as_int())

    def post_ready(self, h: fc.BaseHandle) -> None:
        logger.debug(f"Re-notifying for channel: {h.as_int()}")
        self.handle_ready_queues[h.as_int()].put_nowait(h.as_int())

    async def wait_ready(self, h: fc.BaseHandle) -> int:
        res = await self.handle_ready_queues[h.as_int()].get()
        self.handle_ready_queues[h.as_int()].task_done()
        return res
