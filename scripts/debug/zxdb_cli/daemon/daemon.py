# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import contextlib
import os
import signal
from collections.abc import Awaitable, Callable, Generator
from pathlib import Path
from typing import Any, Final, TypeVar, cast, final

from async_utils.command import AsyncCommand
from ffx_cmd.lib import FfxCmd
from pydap.client import DapClient
from pydap.models import (
    ContinueArguments,
    InitializeArguments,
    PauseArguments,
    StackTraceArguments,
    dataclass_to_dict,
)
from shared.protocol import (
    PROTOCOL_VERSION,
    AttachRequest,
    BaseRequest,
    ContinueRequest,
    GetStateRequest,
    HelloRequest,
    PauseRequest,
    Response,
    StackTraceRequest,
    StopRequest,
    ThreadsRequest,
    deserialize_request,
    serialize,
)

# TODO(https://fxbug.dev/504962182): Replace this with something more appropriate.
UDS_PATH: Final[Path] = Path("/tmp/fx-debug-daemon.sock")

DEFAULT_DAP_PORT: Final[int] = 15678


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
        connect_to_existing: bool = False,
        ready_fd: int | None = None,
    ) -> None:
        self.registry = CommandHandlerRegistry()
        self.dap_client = DapClient()
        self.background_tasks: set[asyncio.Task[None]] = set()
        self.active_handlers: set[asyncio.Task[Any]] = set()
        self.event_queue: asyncio.Queue[Any] = asyncio.Queue()
        self.event_waiter = DapEventWaiter()
        self.stopped_threads: set[int] = set()
        self.stop_event = asyncio.Event()
        self.shutdown_complete_event = asyncio.Event()
        self.dap_ready_event = asyncio.Event()
        self.zxdb_writer: asyncio.StreamWriter | None = None
        self.zxdb_reader: asyncio.StreamReader | None = None
        self.port = port
        self.connect_to_existing = connect_to_existing
        self.dap_proc: AsyncCommand | None = None
        self.ready_fd = ready_fd

        self.registry.register("stop", self.handle_stop)
        self.registry.register("hello", self.handle_hello)
        self.registry.register(
            "get-state",
            self.handle_get_state,
        )
        self.registry.register(
            "attach",
            self.handle_attach,
        )
        self.registry.register(
            "threads",
            self.handle_threads,
        )
        self.registry.register(
            "continue",
            self.handle_continue,
        )
        self.registry.register(
            "pause",
            self.handle_pause,
        )
        self.registry.register(
            "stackTrace",
            self.handle_stack_trace,
        )

    # TODO(https://fxbug.dev/509967647): This should be part of a thread object abstraction that
    # we're keeping track of.
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

    async def handle_stop(self, _req: StopRequest) -> Response:
        self.stop_event.set()
        return Response(success=True, message="Daemon stopping")

    async def handle_hello(self, req: HelloRequest) -> Response:
        """Handles the hello handshake request.

        Verifies the protocol version and blocks until the DAP server
        connection is fully initialized.
        """
        if req.version != PROTOCOL_VERSION:
            return Response(
                success=False,
                message=f"Protocol version mismatch. CLI version: {req.version}, Daemon version: {PROTOCOL_VERSION}",
            )

        try:
            await asyncio.wait_for(self.dap_ready_event.wait(), timeout=10.0)
        except asyncio.TimeoutError:
            return Response(
                success=False,
                message="Timed out waiting for DAP connection to be ready",
            )

        return Response(
            success=True, body={"protocol_version": PROTOCOL_VERSION}
        )

    async def handle_get_state(self, _req: GetStateRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )
        try:
            threads_resp = await self.dap_client.threads(self.zxdb_writer)
            threads = []
            for t in threads_resp.threads:
                threads.append(
                    {
                        "id": t.id,
                        "name": t.name,
                    }
                )
            return Response(success=True, body={"threads": threads})
        except Exception as e:
            return Response(
                success=False, message=f"Failed to get threads: {e}"
            )

    async def handle_attach(self, req: AttachRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )

        from pydap.models import AttachRequestArguments

        attach_args = AttachRequestArguments(
            _restart=None, extra_fields={"process": req.filter}
        )

        try:
            resp = await self.dap_client.attach(self.zxdb_writer, attach_args)
            return Response(success=True, body=resp)
        except Exception as e:
            return Response(success=False, message=f"Failed to attach: {e}")

    async def handle_threads(self, _req: ThreadsRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )

        try:
            resp = await self.dap_client.threads(self.zxdb_writer)
            return Response(success=True, body=dataclass_to_dict(resp))
        except Exception as e:
            return Response(
                success=False, message=f"Failed to get threads: {e}"
            )

    async def handle_continue(self, req: ContinueRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )

        args = ContinueArguments(
            threadId=req.thread_id, singleThread=req.single_thread
        )

        try:
            resp = await self.dap_client.continue_thread(self.zxdb_writer, args)
            return Response(success=True, body=resp)
        except Exception as e:
            return Response(success=False, message=f"Failed to continue: {e}")

    async def handle_pause(self, req: PauseRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )

        try:
            await self.ensure_stopped(req.thread_id)
            return Response(success=True)
        except Exception as e:
            return Response(success=False, message=f"Failed to pause: {e}")

    async def handle_stack_trace(self, req: StackTraceRequest) -> Response:
        if not self.zxdb_writer:
            return Response(
                success=False, message="Not connected to zxdb DAP server"
            )

        try:
            await self.ensure_stopped(req.thread_id)

            # Now thread is paused, get stack trace
            stack_resp = await self.dap_client.stack_trace(
                self.zxdb_writer,
                StackTraceArguments(
                    threadId=req.thread_id,
                ),
            )

            return Response(success=True, body=dataclass_to_dict(stack_resp))
        except Exception as e:
            return Response(
                success=False, message=f"Failed to get stack trace: {e}"
            )

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

        if not self.connect_to_existing:
            import package_server

            async with package_server.ensure_running():
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
                else:
                    self.port = DEFAULT_DAP_PORT

                self.dap_proc = await ffx_cmd.start(*args)

                # Wait for signal from zxdb
                loop = asyncio.get_running_loop()
                signal_fut = loop.create_future()

                def handle_sigusr1() -> None:
                    signal_fut.set_result(True)

                loop.add_signal_handler(signal.SIGUSR1, handle_sigusr1)

                try:
                    await asyncio.wait_for(signal_fut, timeout=30.0)
                    print("Received SIGUSR1 from zxdb.")
                except asyncio.TimeoutError:
                    print("Timed out waiting for SIGUSR1 from zxdb.")
                    if self.dap_proc:
                        self.dap_proc.terminate()
                    server.close()
                    await server.wait_closed()
                    UDS_PATH.unlink(missing_ok=True)
                    return 1
                finally:
                    loop.remove_signal_handler(signal.SIGUSR1)

                return await self._run_dap_session(server)
        else:
            return await self._run_dap_session(server)

    async def _run_dap_session(self, server: asyncio.AbstractServer) -> int:
        # Poll for connection to DAP port
        connected = False
        for _ in range(20):
            if self.stop_event.is_set():
                print("Stop event set during DAP polling. Exiting.")
                return 0
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
                    print("Stop event set during DAP polling wait. Exiting.")
                    return 0
                except asyncio.TimeoutError:
                    pass

        if not connected:
            print("Failed to connect to DAP server after polling.")
            if self.dap_proc:
                self.dap_proc.terminate()
            server.close()
            await server.wait_closed()
            UDS_PATH.unlink(missing_ok=True)
            return 1

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
        self.dap_ready_event.set()

        await self.stop_event.wait()

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

        self.shutdown_complete_event.set()
        return 0

    async def handle_uds_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        # This function is only called when there is a new connection. Each connection is only
        # expected to send a single request so there is no looping to do here. At the start of a new
        # connection we store this task so that the main task can be sure that there are no dangling
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

            resp = await self.registry.handle(req.command, req)
            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()

            if req.command == "stop":
                await self.shutdown_complete_event.wait()
        except Exception as e:
            resp = Response(success=False, message=f"Error: {e}")
            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()
        finally:
            self.active_handlers.discard(current_task)
            writer.close()
            await writer.wait_closed()

    async def _process_events(self) -> None:
        while True:
            event = await self.event_queue.get()
            if event.get("event") == "stopped":
                thread_id = event.get("body", {}).get("threadId")
                self.stopped_threads.add(thread_id)
                self.event_waiter.notify_thread_stop(thread_id, event)
            elif event.get("event") == "continued":
                thread_id = event.get("body", {}).get("threadId")
                self.stopped_threads.discard(thread_id)
