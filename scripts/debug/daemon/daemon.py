# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import os
import signal
from collections.abc import Awaitable, Callable
from typing import Any, Final, TypeVar, cast, final

from async_utils.command import AsyncCommand
from ffx_cmd.lib import FfxCmd
from pydap.client import DapClient
from shared.protocol import (
    BaseRequest,
    GetStateRequest,
    Response,
    StopRequest,
    deserialize_request,
    serialize,
)

# TODO(https://fxbug.dev/504962182): Replace this with something more appropriate.
UDS_PATH = "/tmp/fx-debug-daemon.sock"

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


@final
class Daemon:
    def __init__(
        self, port: int | None, connect_to_existing: bool = False
    ) -> None:
        self.registry = CommandHandlerRegistry()
        self.dap_client = DapClient()
        self.background_tasks: set[asyncio.Task[None]] = set()
        self.active_handlers: set[asyncio.Task[Any]] = set()
        self.event_queue: asyncio.Queue[Any] = asyncio.Queue()
        self.stop_event = asyncio.Event()
        self.zxdb_writer: asyncio.StreamWriter | None = None
        self.zxdb_reader: asyncio.StreamReader | None = None
        self.port = port
        self.connect_to_existing = connect_to_existing
        self.dap_proc: AsyncCommand | None = None

        self.registry.register("stop", self.handle_stop)
        self.registry.register(
            "get-state",
            self.handle_get_state,
        )

    async def handle_stop(self, _req: StopRequest) -> Response:
        self.stop_event.set()
        return Response(success=True, message="Daemon stopping")

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

    async def run(self) -> int:
        if os.path.exists(UDS_PATH):
            os.unlink(UDS_PATH)

        server = await asyncio.start_unix_server(
            self.handle_uds_client, UDS_PATH
        )
        print(f"Daemon listening on {UDS_PATH}")

        if not self.connect_to_existing:
            import package_server

            async with package_server.ensure_running():
                ffx_cmd = FfxCmd()
                pid = os.getpid()
                args = [
                    "debug",
                    "connect",
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
                    os.unlink(UDS_PATH)
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
            try:
                (
                    self.zxdb_reader,
                    self.zxdb_writer,
                ) = await asyncio.open_connection("localhost", self.port)
                connected = True
                print("Connected to DAP server.")
                break
            except Exception:
                await asyncio.sleep(1)

        assert self.zxdb_reader is not None
        assert self.zxdb_writer is not None

        if not connected:
            print("Failed to connect to DAP server after polling.")
            if self.dap_proc:
                self.dap_proc.terminate()
            server.close()
            await server.wait_closed()
            os.unlink(UDS_PATH)
            return 1

        # Run DAP client
        self.background_tasks.add(
            asyncio.create_task(
                self.dap_client.run(self.zxdb_reader, self.event_queue)
            )
        )

        # Initialize DAP
        from pydap.models import InitializeArguments

        await self.dap_client.initialize(
            self.zxdb_writer,
            InitializeArguments(adapterID="zxdb"),
        )

        await self.stop_event.wait()

        _done, pending = await asyncio.wait(self.active_handlers, timeout=5.0)
        for task in pending:
            task.cancel()

        server.close()
        await server.wait_closed()
        if os.path.exists(UDS_PATH):
            os.unlink(UDS_PATH)

        if self.dap_proc:
            self.dap_proc.terminate()

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

        self.active_handlers.add(current_task)

        line = await reader.readline()
        if not line:
            return

        try:
            req = deserialize_request(line.decode("utf-8"))
            resp = await self.registry.handle(req.command, req)
            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()
        except Exception as e:
            resp = Response(success=False, message=f"Error: {e}")
            writer.write(serialize(resp).encode("utf-8"))
            await writer.drain()
        finally:
            self.active_handlers.remove(current_task)
            writer.close()
            await writer.wait_closed()
