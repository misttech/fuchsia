# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import contextlib
import functools
import os
import signal
import uuid
from collections.abc import Awaitable, Callable, Generator
from typing import Any, Final, TypeVar, cast, final

import package_server
from async_utils.command import AsyncCommand
from daemon.constants import UDS_PATH
from daemon.handlers import (
    attach,
    continue_req,
    detach,
    get_state,
    hello,
    pause,
    stack_trace,
    start,
    stop,
    threads,
    wait_for_event,
)
from ffx_cmd.lib import FfxCmd
from pydap.models import InitializeArguments, PauseArguments
from shared.protocol import (
    BaseRequest,
    Response,
    deserialize_request,
    serialize,
)
from shared.protocol.start import StartRequest
from zxdb_dap import ZxdbDapClient, ZxdbDetachArguments

# Constants imported from daemon.constants
MAX_EVENT_HISTORY_SIZE: Final[int] = 100


class CommandHandlerRegistry:
    def __init__(self) -> None:
        self.handlers: dict[
            str, Callable[[BaseRequest], Awaitable[Response]]
        ] = {}

    RequestT = TypeVar("RequestT", bound=BaseRequest)

    def register(
        self,
        command: str,
        handler: Callable[[RequestT], Awaitable[Response]],
    ) -> None:
        self.handlers[command] = cast(
            Callable[[BaseRequest], Awaitable[Response]], handler
        )

    async def handle(self, command: str, req: BaseRequest) -> Response:
        if command in self.handlers:
            try:
                return await self.handlers[command](req)
            except Exception as e:
                return Response(success=False, message=f"Handler error: {e}")
        return Response(success=False, message=f"Unknown command: {command}")


class DapEventWaiter:
    """Manages futures for tasks waiting for specific DAP events."""

    def __init__(self) -> None:
        self._waiters: dict[
            tuple[str, int], list[asyncio.Future[dict[str, Any]]]
        ] = {}

    def register_thread_stop(
        self, thread_id: int
    ) -> asyncio.Future[dict[str, Any]]:
        fut = asyncio.get_running_loop().create_future()
        key = ("stopped", thread_id)
        self._waiters.setdefault(key, []).append(fut)
        return fut

    def unregister_thread_stop(
        self, thread_id: int, fut: asyncio.Future[dict[str, Any]]
    ) -> None:
        key = ("stopped", thread_id)
        if key in self._waiters:
            if fut in self._waiters[key]:
                self._waiters[key].remove(fut)
            if not self._waiters[key]:
                del self._waiters[key]

    def notify_thread_stop(self, thread_id: int, event: dict[str, Any]) -> None:
        key = ("stopped", thread_id)
        if key in self._waiters:
            for fut in self._waiters[key]:
                if not fut.done():
                    fut.set_result(event)
            del self._waiters[key]

    @contextlib.contextmanager
    def wait_for_thread_stop(
        self, thread_id: int
    ) -> Generator[asyncio.Future[dict[str, Any]], None, None]:
        """Context manager to automatically register and unregister a waiter."""
        fut = self.register_thread_stop(thread_id)
        try:
            yield fut
        finally:
            self.unregister_thread_stop(thread_id, fut)


@final
class Daemon:
    def __init__(
        self,
        port: int | None,
        ready_fd: int | None = None,
    ) -> None:
        self.registry = CommandHandlerRegistry()
        self.dap_client = ZxdbDapClient()
        self.background_tasks: set[asyncio.Task[None]] = set()
        self.active_handlers: set[asyncio.Task[Any]] = set()
        self.event_queue: asyncio.Queue[Any] = asyncio.Queue()
        self.event_waiter = DapEventWaiter()
        self.stopped_threads: set[int] = set()
        self.stop_event = asyncio.Event()
        self.shutdown_complete_event = asyncio.Event()
        self.dap_ready_event = asyncio.Event()
        self.dap_initialized_event = asyncio.Event()
        self._start_lock = asyncio.Lock()
        # We use a regular dict to store events, keyed by sequence number which
        # preserves insertion order. This is relevant because it allows us to
        # efficiently prune old events by iterating from the beginning of the
        # dict keys and breaking early when we reach a key that shouldn't be
        # pruned yet.
        self.all_events: dict[int, dict[str, Any]] = {}
        self.latest_seq = 0
        self.new_event_condition = asyncio.Condition()
        self.zxdb_writer: asyncio.StreamWriter | None = None
        self.zxdb_reader: asyncio.StreamReader | None = None
        self.port = port
        self.connect_to_existing: bool | None = None
        self.active_processes: dict[int, str] = {}
        self.dap_proc: AsyncCommand | None = None
        self.package_server_proc: Any = None
        self.repo_name: str | None = None
        self.ready_fd = ready_fd

        # Statically register handlers.
        handlers = [
            attach,
            continue_req,
            detach,
            get_state,
            hello,
            pause,
            stack_trace,
            start,
            stop,
            threads,
            wait_for_event,
        ]
        for mod in handlers:
            handler_fn = functools.partial(mod.handle, self)
            self.registry.register(mod.COMMAND_NAME, handler_fn)

    # TODO(https://fxbug.dev/509967647): This should be part of a thread object
    # abstraction that we're keeping track of.
    async def ensure_stopped(self, thread_id: int) -> None:
        """Ensures the thread is stopped. Returns immediately if it is,
        otherwise pauses it and waits for the stopped event.
        """
        if thread_id in self.stopped_threads:
            return

        if not self.zxdb_writer:
            raise Exception("Not connected to zxdb DAP server")

        with self.event_waiter.wait_for_thread_stop(thread_id) as fut:
            await self.dap_client.pause_thread(
                self.zxdb_writer, PauseArguments(threadId=thread_id)
            )
            try:
                await asyncio.wait_for(fut, timeout=10.0)
                return
            except asyncio.TimeoutError:
                raise Exception(
                    f"Timed out waiting for thread {thread_id} to stop"
                )

    def _check_already_running(self, req: StartRequest) -> Response | None:
        if not self.dap_ready_event.is_set():
            return None

        if (
            req.port is not None
            and self.port is not None
            and req.port != self.port
        ):
            return Response(
                success=False,
                message=(
                    f"Daemon already running on port {self.port}, "
                    f"cannot switch to {req.port}"
                ),
            )
        if req.connect != self.connect_to_existing:
            return Response(
                success=False,
                message=(
                    f"Daemon already running with "
                    f"connect_to_existing={self.connect_to_existing}, "
                    f"cannot switch to {req.connect}"
                ),
            )
        return Response(
            success=True,
            body={"uds_path": str(UDS_PATH)},
            message="Daemon already started",
        )

    async def _start_dap_server(self) -> Response | None:
        if not await package_server.is_running():
            self.repo_name = f"tmp-{uuid.uuid4()}"
            try:
                self.package_server_proc = await package_server.start(
                    self.repo_name
                )
            except Exception as e:
                return Response(
                    success=False,
                    message=f"Failed to start package server: {e}",
                )
        else:
            self.package_server_proc = None
            self.repo_name = None

        ffx_cmd = FfxCmd()
        pid = os.getpid()
        args = [
            "debug",
            "connect",
            "--new-agent",
            "--",
            "--enable-debug-adapter",
            f"--signal-when-ready={pid}",
        ]
        if self.port is not None:
            args.extend(["--debug-adapter-port", str(self.port)])

        try:
            self.dap_proc = await ffx_cmd.start(*args)
        except Exception as e:
            return Response(success=False, message=f"Failed to start zxdb: {e}")

        # Wait for signal from zxdb
        loop = asyncio.get_running_loop()
        signal_fut = loop.create_future()

        def handle_sigusr1() -> None:
            signal_fut.set_result(True)

        loop.add_signal_handler(signal.SIGUSR1, handle_sigusr1)

        try:
            await asyncio.wait_for(signal_fut, timeout=30.0)
            print("Received SIGUSR1 from zxdb.")
            return None
        except asyncio.TimeoutError:
            print("Timed out waiting for SIGUSR1 from zxdb.")
            return Response(
                success=False,
                message="Timed out waiting for SIGUSR1 from zxdb",
            )
        finally:
            loop.remove_signal_handler(signal.SIGUSR1)

    async def run(self) -> int:
        if UDS_PATH.exists():
            UDS_PATH.unlink()

        server = await asyncio.start_unix_server(
            self.handle_uds_client, UDS_PATH
        )
        print(f"Daemon listening on {UDS_PATH}")

        if self.ready_fd is not None:
            try:
                os.write(self.ready_fd, b"1")
                os.close(self.ready_fd)
            except OSError as e:
                print(f"Failed to write to ready-fd: {e}")

        # Wait for stop event (sent by handle_stop)
        await self.stop_event.wait()

        if self.connect_to_existing and self.zxdb_writer:
            try:
                args = ZxdbDetachArguments(all=True)
                await self.dap_client.zxdb_detach(self.zxdb_writer, args)
            except Exception:
                pass

        # Cleanup
        if self.active_handlers:
            _done, pending = await asyncio.wait(
                self.active_handlers, timeout=5.0
            )
            for task in pending:
                task.cancel()

        for task in self.background_tasks:
            task.cancel()

        if self.zxdb_writer:
            self.zxdb_writer.close()
            try:
                await self.zxdb_writer.wait_closed()
            except Exception:
                pass

        server.close()
        await server.wait_closed()
        if UDS_PATH.exists():
            UDS_PATH.unlink(missing_ok=True)

        if self.dap_proc:
            self.dap_proc.terminate()

        if self.package_server_proc:
            self.package_server_proc.terminate()
            await self.package_server_proc.wait()

            if self.repo_name:
                await package_server.stop(self.repo_name)

        self.shutdown_complete_event.set()
        return 0

    async def _connect_to_dap(self) -> bool:
        connected = False
        for _ in range(20):
            if self.stop_event.is_set():
                return False
            try:
                (
                    self.zxdb_reader,
                    self.zxdb_writer,
                ) = await asyncio.open_connection("localhost", self.port)
                connected = True
                print("Connected to DAP server.")
                break
            except Exception:
                try:
                    await asyncio.wait_for(self.stop_event.wait(), timeout=1.0)
                    return False
                except asyncio.TimeoutError:
                    pass

        if not connected:
            print("Failed to connect to DAP server after polling.")
            return False

        assert self.zxdb_reader is not None
        assert self.zxdb_writer is not None

        # Run DAP client
        self.background_tasks.add(
            asyncio.create_task(
                self.dap_client.run(self.zxdb_reader, self.event_queue)
            )
        )

        self.background_tasks.add(asyncio.create_task(self._process_events()))

        await self.dap_client.initialize(
            self.zxdb_writer,
            InitializeArguments(adapterID="zxdb"),
        )

        # Wait for the "initialized" event from the DAP server.
        try:
            await asyncio.wait_for(
                self.dap_initialized_event.wait(), timeout=5.0
            )
            print("Received 'initialized' event from DAP server.")
        except asyncio.TimeoutError:
            print("Timed out waiting for 'initialized' event from DAP server.")
            return False
        except Exception as e:
            print(f"Error waiting for 'initialized' event: {e}")
            return False

        self.dap_ready_event.set()
        return True

    async def handle_uds_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        # This function is only called when there is a new connection. Each
        # connection is only expected to send a single request so there is no
        # looping to do here. At the start of a new connection we store this
        # task so that the main task can be sure that there are no dangling
        # connections during shutdown.
        current_task = asyncio.current_task()
        assert current_task is not None

        line = await reader.readline()
        if not line:
            return

        try:
            req = deserialize_request(line.decode("utf-8"))

            # To avoid deadlocks during shutdown, do not add the "stop" command
            # handler task to active_handlers. The main loop in _run_dap_session
            # waits for all active_handlers to complete before shutting down.
            # If the stop handler task waits for shutdown inside active_handlers,
            # it would wait for itself to complete.
            if req.command != "stop":
                self.active_handlers.add(current_task)

            if req.ack_seq is not None:
                # Note: this relies on the insertion order of dictionaries being preserved.
                for seq in list(self.all_events.keys()):
                    if seq <= req.ack_seq:
                        del self.all_events[seq]
                    else:
                        break

            resp = await self.registry.handle(req.command, req)

            # Add events that have transpired since |last_seen_seq|.
            if req.last_seen_seq is not None:
                resp.events = []
                for seq in range(req.last_seen_seq + 1, self.latest_seq + 1):
                    if seq in self.all_events:
                        resp.events.append(self.all_events[seq])

            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()

            if req.command == "stop":
                await self.shutdown_complete_event.wait()
        except asyncio.CancelledError:
            raise
        except Exception as e:
            resp = Response(success=False, message=f"Error: {e}")
            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()
        finally:
            self.active_handlers.discard(current_task)
            writer.close()
            await writer.wait_closed()

    async def _process_events(self) -> None:
        allowed_events = {
            "stopped",
            "continued",
            "exited",
            "terminated",
            "thread",
            "process",
            "detached",
        }
        while True:
            event = await self.event_queue.get()

            # Internal daemon actions on all events
            if event.get("event") == "initialized":
                self.dap_initialized_event.set()
            elif event.get("event") == "stopped":
                thread_id = event.get("body", {}).get("threadId")
                self.stopped_threads.add(thread_id)
                self.event_waiter.notify_thread_stop(thread_id, event)
            elif event.get("event") == "continued":
                thread_id = event.get("body", {}).get("threadId")
                self.stopped_threads.discard(thread_id)
            elif event.get("event") == "process":
                body = event.get("body", {})
                pid = body.get("systemProcessId")
                name = body.get("name")
                if pid is not None:
                    self.active_processes[pid] = name
            elif event.get("event") in ("exited", "terminated"):
                # Assume single process for now, clear all.
                self.active_processes.clear()

            # Only enqueue and sequence allowed events for surfacing to the CLI client
            if event.get("event") in allowed_events:
                self.latest_seq += 1
                event["seq"] = self.latest_seq
                self.all_events[self.latest_seq] = event

                # Enforce max size
                if len(self.all_events) > MAX_EVENT_HISTORY_SIZE:
                    # Pop the oldest item. Dict keys are in insertion order.
                    oldest_seq = next(iter(self.all_events))
                    del self.all_events[oldest_seq]

                async with self.new_event_condition:
                    self.new_event_condition.notify_all()
