# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging
from abc import abstractmethod
from inspect import getframeinfo, stack
from typing import Any, Coroutine, Dict, Set, cast

import fuchsia_controller_py as fc
from fidl_codec import decode_fidl_response, encode_fidl_message

from ._fidl_common import (
    FIDL_EPITAPH_ORDINAL,
    EpitaphError,
    FidlMessage,
    FidlMeta,
    StopEventHandler,
    TXID_Type,
    parse_epitaph_value,
    parse_ordinal,
    parse_txid,
)
from ._ipc import GlobalHandleWaker, HandleWaker

# The active TXID (mutable).
TXID: TXID_Type = 0

# The TXID of a FIDL event.
EVENT_TXID: TXID_Type = 0
# Simple client ID. Monotonically increasing for each client.
_CLIENT_ID = 0
_LOGGER = logging.getLogger("fidl.client")


class FidlClient(metaclass=FidlMeta):
    @staticmethod
    @abstractmethod
    def construct_response_object(
        response_ident: str, response_obj: Any
    ) -> Any:
        ...

    def __init__(
        self,
        channel: int | fc.Channel,
        channel_waker: HandleWaker | None = None,
    ) -> None:
        global _CLIENT_ID
        self.id = _CLIENT_ID
        _CLIENT_ID += 1
        if isinstance(channel, int):
            self._channel: fc.Channel | None = fc.Channel(channel)
        else:
            self._channel = channel
        if channel_waker is None:
            self._channel_waker: HandleWaker = GlobalHandleWaker()
        else:
            self._channel_waker = channel_waker
        self.pending_txids: Set[TXID_Type] = set({})
        self.staged_messages: Dict[TXID_Type, asyncio.Queue[FidlMessage]] = {}
        self.epitaph_received: EpitaphError | None = None
        self.epitaph_event = asyncio.Event()
        caller = getframeinfo(stack()[1][0])
        _LOGGER.debug(
            f"{self} instantiated from {caller.filename}:{caller.lineno}"
        )

    def close_cleanly(self) -> None:
        """Closes the underlying channel.

        This is so-named to avoid name conflicts with existing FIDL methods.
        """
        if self._channel is not None:
            self._channel.close()
            # self._channel = None is NOT done here, as it may be used in pending tasks.
            # The handle being closed will result in errors in those tasks.

    def __str__(self) -> str:
        return f"client:{type(self).__name__}:{self.id}"

    async def _get_staged_message(self, txid: TXID_Type) -> FidlMessage:
        res = await self.staged_messages[txid].get()
        self.staged_messages[txid].task_done()
        return res

    def _stage_message(self, txid: TXID_Type, msg: FidlMessage) -> None:
        # This should only ever happen if we're a channel reading another channel's response before
        # it has ever made a request.
        if txid not in self.staged_messages:
            self.staged_messages[txid] = asyncio.Queue(1)
        self.staged_messages[txid].put_nowait(msg)

    def _clean_staging(self, txid: TXID_Type) -> None:
        self.staged_messages.pop(txid)
        # Events are never added to this set, since they're always pending.
        if txid != EVENT_TXID:
            self.pending_txids.remove(txid)

    def _decode(self, txid: TXID_Type, msg: FidlMessage) -> Dict[str, Any]:
        self._clean_staging(txid)
        handles = msg[1]
        verified_handles: list[int] = [0] * len(handles)
        for i in range(len(handles)):
            # Asserting is not enough for mypy, we must also cast.
            hdl = cast(fc.BaseHandle, handles[i])
            assert isinstance(hdl, fc.BaseHandle)
            verified_handles[i] = hdl.take()
        return decode_fidl_response(bytes=msg[0], handles=verified_handles)

    async def next_event(self) -> FidlMessage | None:
        """Attempts to read the next FIDL event from this client.

        Returns:
            The next FIDL event. If ZX_ERR_PEER_CLOSED is received on the channel, will return None.
            Note: this does not check to see if the protocol supports any events, so if not this
            function could wait forever.

        Raises:
            Any exceptions other than ZX_ERR_PEER_CLOSED (fuchsia_controller_py.FcTransportStatus)
        """
        # TODO(awdavies): Raise an exception if there are no events supported for this client.
        try:
            return await self._read_and_decode(0)
        except fc.FcTransportStatus as e:
            err_code = e.code()
            if err_code != fc.FcTransportStatus.FC_ERR_FDOMAIN:
                _LOGGER.warning(
                    f"{self} received error waiting for next event: {e}"
                )
                raise e
        return None

    def _epitaph_check(self, msg: FidlMessage) -> None:
        # If the epitaph is already set, no need to continue with the remaining
        # work.
        if self.epitaph_received is not None:
            raise self.epitaph_received

        ordinal = parse_ordinal(msg)
        if ordinal == FIDL_EPITAPH_ORDINAL:
            if self.epitaph_received is None:
                self.epitaph_received = EpitaphError(parse_epitaph_value(msg))
                self.epitaph_event.set()
            if self.epitaph_received is not None:
                raise self.epitaph_received

    async def _epitaph_event_wait(self) -> None:
        await self.epitaph_event.wait()
        if self.epitaph_received is not None:
            raise self.epitaph_received

    # TODO(https://fxbug.dev/493309088): This (and many of the other `-> Any` return
    # values should be returning a specific type, as we're decoding a nested
    # dictionary type that is specific to FIDL.
    async def _read_and_decode(self, txid: int) -> Any:
        if txid not in self.staged_messages:
            self.staged_messages[txid] = asyncio.Queue(1)
        if self._channel is None:
            raise ValueError("Channel is already closed")
        msg: FidlMessage
        with self._channel_waker.registration(self._channel, name=str(self)):
            while True:
                if self.epitaph_received is not None:
                    raise self.epitaph_received

                # 1. Try to read from the channel.
                try:
                    msg = self._channel.read()
                    self._epitaph_check(msg)
                    recvd_txid = parse_txid(msg)
                    if recvd_txid == txid:
                        if txid != EVENT_TXID:
                            return self._decode(txid, msg)
                        else:
                            self._clean_staging(txid)
                            return msg

                    if (
                        recvd_txid != EVENT_TXID
                        and recvd_txid not in self.pending_txids
                    ):
                        _LOGGER.warning(
                            f"{self} received unexpected TXID: {recvd_txid}"
                        )
                        # Unexpected TXID is often a sign of a bad server or a serious bug.
                        # We don't close the channel here, but we raise.
                        raise RuntimeError(
                            f"{self} received unexpected TXID: {recvd_txid}"
                        )

                    self._stage_message(recvd_txid, msg)
                except fc.FcTransportStatus as e:
                    if e.code() != fc.FcTransportStatus.FC_ERR_SHOULD_WAIT:
                        raise e

                # 2. Wait for either:
                #    - The channel being readable again.
                #    - A staged message for our TXID becoming available.
                #    - An epitaph being received.
                #
                #    This is handled with a task group in the event that
                #    there are task cancellations they apply to the other
                #    tasks in the task group, making cleanup predictable.
                try:
                    async with asyncio.TaskGroup() as tg:
                        read_ready_task = asyncio.create_task(
                            self._channel_waker.wait_ready(self._channel)
                        )
                        staged_msg_task = asyncio.create_task(
                            self._get_staged_message(txid)
                        )
                        epitaph_task = asyncio.create_task(
                            self.epitaph_event.wait()
                        )

                        done, pending = await asyncio.wait(
                            [read_ready_task, staged_msg_task, epitaph_task],
                            return_when=asyncio.FIRST_COMPLETED,
                        )

                        for p in pending:
                            p.cancel()
                except ExceptionGroup as eg:
                    # If there is only a single exception, raise this as the
                    # main exception. We care particularly if there is an
                    # epitaph exception raised.
                    if len(eg.exceptions) == 1:
                        raise eg.exceptions[0] from None
                    raise

                if staged_msg_task in done:
                    # If we got a staged message, we're done.
                    msg = staged_msg_task.result()
                    # If read_ready_task was also done, we should "re-notify" because we didn't read.
                    if read_ready_task in done:
                        if self._channel is None:
                            raise ValueError("Channel is already closed")
                        self._channel_waker.post_ready(self._channel)

                    if txid != EVENT_TXID:
                        return self._decode(txid, msg)
                    else:
                        self._clean_staging(txid)
                        return msg

                if epitaph_task in done:
                    # This will raise the epitaph error.
                    if self.epitaph_received is not None:
                        raise self.epitaph_received
                    return None

                # If only read_ready_task reached here, we just loop again and try to read().

    def _send_two_way_fidl_request(
        self,
        ordinal: int,
        library: str,
        msg_obj: Any,
        response_ident: str,
    ) -> Coroutine[Any, Any, Any]:
        """Sends a two-way asynchronous FIDL request.

        Args:
            ordinal: The method ordinal (for encoding).
            library: The FIDL library from which this method ordinal exists.
            msg_obj: The object being sent.
            response_ident: The full FIDL identifier of the response object, e.g. foo.bar/Baz

        Returns:
            The object from the two-way function, as constructed from the response_ident type.
        """
        global TXID
        TXID += 1
        self.pending_txids.add(TXID)
        self._send_one_way_fidl_request(TXID, ordinal, library, msg_obj)

        async def result(txid: int) -> Any:
            res = await self._read_and_decode(txid)
            return self.construct_response_object(response_ident, res)

        return result(TXID)

    def _send_one_way_fidl_request(
        self, txid: int, ordinal: int, library: str, msg_obj: Any
    ) -> None:
        """Sends a synchronous one-way FIDL request.

        Args:
            ordinal: The method ordinal (for encoding).
            library: The FIDL library from which this method ordinal exists.
            msg_obj: The object being sent.
        """
        type_name = None
        if msg_obj is not None:
            type_name = msg_obj.__fidl_raw_type__
        encoded_fidl_message = encode_fidl_message(
            ordinal=ordinal,
            object=msg_obj,
            library=library,
            txid=txid,
            type_name=type_name,
        )
        if self._channel is None:
            raise ValueError("Channel is already closed")
        self._channel.write(encoded_fidl_message)


class EventHandlerBase(
    metaclass=FidlMeta,
    required_class_variables=[
        ("library", str),
        ("method_map", dict),
    ],
):
    """Base object for doing FIDL client event handling."""

    client: FidlClient
    method_map: Dict[int, Any]

    @staticmethod
    @abstractmethod
    def construct_response_object(
        response_ident: str, response_obj: Any
    ) -> Any:
        ...

    def __init__(self, client: FidlClient) -> None:
        self.client = client

    def __str__(self) -> str:
        return f"event:{type(self.client).__name__}:{self.client.id}"

    async def serve(self) -> None:
        while True:
            msg = await self.client.next_event()
            # msg is None if the channel has been closed.
            if msg is None:
                break
            if not await self._handle_request(msg):
                break

    async def _handle_request(self, msg: FidlMessage) -> bool:
        try:
            await self._handle_request_helper(msg)
            return True
        except StopEventHandler:
            return False

    async def _handle_request_helper(self, msg: FidlMessage) -> None:
        ordinal = parse_ordinal(msg)
        handles = msg[1]
        verified_handles: list[int] = [0] * len(handles)
        for i in range(len(handles)):
            # Asserting is not enough for mypy, we must also cast.
            hdl = cast(fc.BaseHandle, handles[i])
            assert isinstance(hdl, fc.BaseHandle)
            verified_handles[i] = hdl.take()
        decoded_msg = decode_fidl_response(
            bytes=msg[0], handles=verified_handles
        )
        method = self.method_map[ordinal]
        request_ident = method.request_ident
        request_obj = self.construct_response_object(request_ident, decoded_msg)
        method_lambda = getattr(self, method.name)
        if request_obj is not None:
            res = method_lambda(request_obj)
        else:
            res = method_lambda()
        if asyncio.iscoroutine(res) or asyncio.isfuture(res):
            await res
