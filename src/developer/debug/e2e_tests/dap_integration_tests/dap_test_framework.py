# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import asyncio
import copy
import json
import os
import signal
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable, Coroutine, Dict, List, Optional

from async_utils.command import (
    AsyncCommand,
    StderrEvent,
    StdoutEvent,
    TerminationEvent,
)
from ffx_cmd.lib import FfxCmd
from portpicker import portpicker
from pydap.dap_types import DapBaseModel
from pydap.models import (
    EvaluateArguments,
    InitializeArguments,
    LaunchArguments,
    StackTraceArguments,
)
from zxdb_dap import ZxdbDapClient


class RequestFuture:
    """A future representing a pending DAP request and its expectations."""

    def __init__(
        self, framework: DapTestFramework, command: str, request_seq: int
    ) -> None:
        self.framework = framework
        self.command = command
        self.request_seq = request_seq
        self.fut = asyncio.get_running_loop().create_future()
        self.expectations: List[Dict[str, Any]] = []

    def expect(self, partial_check: Dict[str, Any]) -> RequestFuture:
        """Registers a partial check on the response."""
        self.expectations.append(partial_check)
        return self

    def __await__(self) -> Any:
        return self.fut.__await__()

    def set_result(self, result: Any) -> None:
        if self.fut.done():
            return
        try:
            for check in self.expectations:
                self.framework._verify_partial(result, check, raise_error=True)
            self.fut.set_result(result)
        except Exception as e:
            self.fut.set_exception(e)

    def set_exception(self, exc: Exception) -> None:
        if not self.fut.done():
            self.fut.set_exception(exc)


class EventFuture:
    """A future representing a pending DAP event and its expectations."""

    def __init__(self, framework: DapTestFramework, event_name: str) -> None:
        self.framework = framework
        self.event_name = event_name
        self.fut = asyncio.get_running_loop().create_future()
        self.checks: List[Dict[str, Any]] = []

    def expect(self, partial_check: Dict[str, Any]) -> EventFuture:
        """Registers a partial check on the event in O(1) time."""
        self.checks.append(partial_check)
        return self

    def __await__(self) -> Any:
        async def _wait() -> Dict[str, Any]:
            # 1. Check if expectations are already satisfied by past history
            self.framework._check_event_expectations_against_history()

            if self.fut.done():
                return self.fut.result()

            # 2. Await future or wake up immediately if background tasks crash
            tasks = [self.fut]
            if self.framework._client_task:
                tasks.append(self.framework._client_task)
            if self.framework._process_task:
                tasks.append(self.framework._process_task)

            done, _pending = await asyncio.wait(
                tasks, return_when=asyncio.FIRST_COMPLETED
            )

            if self.fut in done:
                return self.fut.result()

            # 3. Check if background tasks crashed
            if (
                self.framework._client_task
                and self.framework._client_task in done
            ):
                exc = self.framework._client_task.exception()
                raise RuntimeError(f"DAP client background task stopped: {exc}")
            if (
                self.framework._process_task
                and self.framework._process_task in done
            ):
                exc = self.framework._process_task.exception()
                raise RuntimeError(
                    f"DAP event processor background task stopped: {exc}"
                )

            return await self.fut

        return _wait().__await__()


def get_build_root() -> Path:
    # //out/default/host_x64/obj/src/developer/debug/e2e_tests/dap_integration_tests/dap_integration_test/dap_integration_test.pyz
    curr = Path(sys.argv[0]).resolve()
    # //out/default
    return curr.parent.parent.parent.parent.parent.parent.parent.parent.parent


def get_ffx_bin() -> str:
    build_root = get_build_root()
    DAP_E2E_TESTS_FFX_TEST_DATA = os.environ.get("DAP_E2E_TESTS_FFX_TEST_DATA")
    if DAP_E2E_TESTS_FFX_TEST_DATA is None:
        raise RuntimeError(
            "DAP_E2E_TESTS_FFX_TEST_DATA environment variable not set"
        )
    # The DAP_E2E_TESTS_FFX_TEST_DATA is calculated by rebase_path(ffx_test_host_tools_out_dir, root_build_dir).
    # root_build_dir is //out/default.
    ffx_bin = str((build_root / DAP_E2E_TESTS_FFX_TEST_DATA / "ffx").resolve())

    if not Path(ffx_bin).exists():
        raise RuntimeError(f"ffx binary not found at: {ffx_bin}")

    return ffx_bin


class DapTestFramework:
    """Base class for DAP integration tests."""

    def __init__(self) -> None:
        self.client = ZxdbDapClient()
        self._split_requests: Dict[int, float] = {}
        self._original_write = self.client._write_message
        setattr(self.client, "_write_message", self._patched_write_message)
        self.event_queue: asyncio.Queue[Dict[str, Any]] = asyncio.Queue()
        self.traffic_history: List[Dict[str, Any]] = []
        self.unmatched_events: List[Dict[str, Any]] = []
        self._client_task: Optional[asyncio.Task[None]] = None
        self._process_task: Optional[asyncio.Task[None]] = None
        self._request_tasks: List[asyncio.Task[None]] = []
        self.proc: Optional[AsyncCommand] = None
        self._server_log_task: Optional[asyncio.Task[None]] = None
        self._writer: Optional[asyncio.StreamWriter] = None
        self.pending_futures: List[RequestFuture] = []
        self.event_expectations: List[EventFuture] = []
        self._isolate_dir: Optional[tempfile.TemporaryDirectory[str]] = None
        self._server_logs: List[str] = []

    async def start_server(self, port: int) -> None:
        """Starts the DAP server via FfxCmd and waits for it to be ready."""
        # Instantiate FfxCmd using create_test_inner() to bypass the default
        # FfxCmd constructor's build directory lookup, which fails in isolated
        # test execution environments like CQ.
        # Ensure FUCHSIA_SSH_KEY is absolute, matching GetFfxEnv() in ffx_debug_agent_bridge.cc
        ssh_key = os.environ.get("FUCHSIA_SSH_KEY")
        if ssh_key:
            os.environ["FUCHSIA_SSH_KEY"] = str(Path(ssh_key).resolve())

        build_root = get_build_root()
        extra_args = []
        self._isolate_dir = tempfile.TemporaryDirectory(
            prefix="ffx_isolate_", dir=os.environ.get("FUCHSIA_TEST_OUTDIR")
        )
        extra_args.extend(["--isolate-dir", self._isolate_dir.name])
        # Replicate GetFfxArgV() config construction
        DAP_E2E_TESTS_FFX_TEST_DATA = os.environ.get(
            "DAP_E2E_TESTS_FFX_TEST_DATA", ""
        )
        ffx_test_data_dir = (build_root / DAP_E2E_TESTS_FFX_TEST_DATA).resolve()

        ffx_config = (
            "log.level=debug,"
            "ffx.isolated=true,"
            "fastboot.usb.disabled=true,"
            "discovery.mdns.enabled=false,"
            f"ffx.subtool-search-paths={ffx_test_data_dir}"
        )

        test_outdir = os.environ.get("FUCHSIA_TEST_OUTDIR")
        if test_outdir:
            ffx_config += f",log.dir={test_outdir}"

        extra_args.extend(["--config", ffx_config])

        device_addr = os.environ.get("FUCHSIA_DEVICE_ADDR", "")
        if device_addr:
            extra_args.append("--target")
            ssh_port = os.environ.get("FUCHSIA_SSH_PORT")
            if ssh_port:
                device_addr = f"{device_addr}:{ssh_port}"
            extra_args.append(device_addr)

        ffx_cmd = FfxCmd(FfxCmd.create_test_inner(get_ffx_bin(), *extra_args))
        args = [
            "debug",
            "connect",
            "--new-agent",
            "--",
            "--enable-debug-adapter",
            "--debug-mode",
            f"--debug-adapter-port={port}",
            f"--signal-when-ready={os.getpid()}",
        ]
        self.proc = await ffx_cmd.start(*args)
        self._server_log_task = asyncio.create_task(self._read_server_log())
        # Setup signal handler to wait for zxdb to be ready
        loop = asyncio.get_running_loop()
        signal_fut = loop.create_future()

        def handle_sigusr1() -> None:
            if not signal_fut.done():
                signal_fut.set_result(True)

        loop.add_signal_handler(signal.SIGUSR1, handle_sigusr1)

        print("Waiting for SIGUSR1 from zxdb...")
        try:
            await asyncio.wait_for(signal_fut, timeout=30.0)
            print("Received SIGUSR1 from zxdb. Server is ready.")
        except asyncio.TimeoutError:
            print("Timed out waiting for SIGUSR1 from zxdb.")
            if self.proc:
                self.proc.terminate()
            raise RuntimeError("Timed out waiting for DAP server to start")
        finally:
            loop.remove_signal_handler(signal.SIGUSR1)
        print(f"Connecting to DAP server on port {port}...")
        reader, writer = await asyncio.open_connection("localhost", port)
        self._writer = writer
        # Start background task for protocol handling
        self._client_task = asyncio.create_task(
            self.client.run(reader, self.event_queue)
        )
        self._process_task = asyncio.create_task(self._event_processor_loop())

    def _send_wrapper(
        self, command: str, coro_fn: Callable[[], Coroutine[Any, Any, Any]]
    ) -> RequestFuture:
        """Executes a DAP client coroutine in the background and returns a RequestFuture."""
        seq = self.client._seq_counter
        req_fut = RequestFuture(self, command, seq)
        self.pending_futures.append(req_fut)

        async def do_send() -> None:
            try:
                resp = await coro_fn()
                assert isinstance(resp, DapBaseModel)
                resp_dict = resp.dump_dap()
                self.traffic_history.append(resp_dict)
                if not resp_dict.get("success", True):
                    raise AssertionError(
                        f"DAP request {command} failed: {resp_dict.get('message')}"
                    )
                req_fut.set_result(resp_dict)
            except Exception as e:
                req_fut.set_exception(e)

        task = asyncio.create_task(do_send())
        self._request_tasks.append(task)
        return req_fut

    def on_event(self, event_name: str) -> EventFuture:
        """Returns an EventFuture to track expectations on events."""
        event_fut = EventFuture(self, event_name)
        self.event_expectations.append(event_fut)
        return event_fut

    async def _patched_write_message(
        self, writer: asyncio.StreamWriter, value: dict[str, Any]
    ) -> None:
        """Intercepts all outbound DAP messages to execute dynamic callbacks on matched sequence numbers."""
        seq = value.get("seq")
        if seq is not None and seq in self._split_requests:
            delay = self._split_requests.pop(seq)
            content = json.dumps(value, separators=(",", ":")).encode("utf-8")
            header = f"Content-Length: {len(content)}\r\n\r\n".encode("utf-8")
            full_payload = header + content
            # Split in half to test arbitrary fragmentation boundaries (e.g. mid-body).
            split_idx = len(full_payload) // 2
            writer.write(full_payload[:split_idx])
            await writer.drain()
            await asyncio.sleep(delay)
            writer.write(full_payload[split_idx:])
            await writer.drain()
        else:
            await self._original_write(writer, value)

    def split_request(self, seq: int, delay: float = 0.1) -> None:
        """Configures the client to split the request with the given sequence number."""
        self._split_requests[seq] = delay

    async def _event_processor_loop(self) -> None:
        """Continuously reads events from queue and processes them against expectations."""
        while True:
            try:
                event = await self.event_queue.get()
                print(f"Framework read event from queue: {event}")
                self.traffic_history.append(event)

                matched = False
                for exp in list(self.event_expectations):
                    if exp.event_name == event.get("event"):
                        all_passed = True
                        for check in exp.checks:
                            if not self._verify_partial(
                                event, check, raise_error=False
                            ):
                                all_passed = False
                                break
                        if all_passed:
                            if not exp.fut.done():
                                exp.fut.set_result(event)
                            self.event_expectations.remove(exp)
                            matched = True
                            break
                if not matched:
                    self.unmatched_events.append(event)
                self.event_queue.task_done()
            except asyncio.CancelledError:
                break
            except Exception as e:
                print(f"Error in event processor loop: {e}")
                raise

    def _verify_partial(
        self,
        data: Dict[str, Any],
        check: Dict[str, Any],
        raise_error: bool = True,
    ) -> bool:
        """Verifies that data matches partial check recursively."""
        for k, v in check.items():
            if k not in data:
                if raise_error:
                    raise AssertionError(
                        f"Expectation failed: key {k} not found in data"
                    )
                return False
            if isinstance(v, dict) and isinstance(data[k], dict):
                if not self._verify_partial(data[k], v, raise_error):
                    return False
            elif isinstance(v, list) and isinstance(data[k], list):
                if not self._verify_list_partial(data[k], v, raise_error):
                    return False
            elif data[k] != v:
                if raise_error:
                    raise AssertionError(
                        f"Expectation failed: expected {k}={v}, got {data[k]}"
                    )
                return False
        return True

    def _verify_list_partial(
        self,
        data_list: List[Any],
        check_list: List[Any],
        raise_error: bool = True,
    ) -> bool:
        """Verifies that each item in check_list partially matches an item in data_list."""
        # Special case: [...] means we expect a list but don't care about its content
        if check_list == [...]:
            return True

        # For each item we expect, search for a matching item in the actual list.
        for check_item in check_list:
            found = False
            for data_item in data_list:
                # If both are dicts, do a recursive partial match.
                if isinstance(check_item, dict) and isinstance(data_item, dict):
                    if self._verify_partial(
                        data_item, check_item, raise_error=False
                    ):
                        found = True
                        break
                # Otherwise, do an exact match.
                elif data_item == check_item:
                    found = True
                    break
            # If any expected item was not found, the expectation fails.
            if not found:
                if raise_error:
                    raise AssertionError(
                        f"Expectation failed: expected item {check_item} not found in list {data_list}"
                    )
                return False
        return True

    def evaluate_and_compare(
        self, golden_file: str, ignore_list: Optional[List[str]] = None
    ) -> None:
        """Compares history against golden file."""
        history = self._get_clean_history(ignore_list)
        with open(golden_file, "r") as f:
            golden = json.load(f)

        if history != golden:
            raise AssertionError(
                f"Snapshot mismatch. History: {history}, Golden: {golden}"
            )

    def evaluate_and_save(
        self, golden_file: str, ignore_list: Optional[List[str]] = None
    ) -> None:
        """Saves history as golden file."""
        history = self._get_clean_history(ignore_list)
        abs_path = os.path.abspath(golden_file)
        print(f"Saving golden file to: {abs_path}")
        os.makedirs(os.path.dirname(abs_path), exist_ok=True)
        with open(abs_path, "w") as f:
            json.dump(history, f, indent=2)

    def _get_clean_history(
        self, ignore_list: Optional[List[str]] = None
    ) -> List[Dict[str, Any]]:
        """Strips volatile fields from history."""
        clean_history = []
        for msg in self.traffic_history:
            clean_msg = copy.deepcopy(msg)
            if ignore_list:
                for path in ignore_list:
                    self._strip_path(clean_msg, path)
            clean_history.append(clean_msg)
        return clean_history

    def _strip_path(self, data: Any, path: str) -> None:
        """Strips a simple dotted path (e.g., 'body.threadId' or '$.seq') from data."""
        # Normalize path (strip leading $ and .)
        if path.startswith("$"):
            path = path[1:]
        if path.startswith("."):
            path = path[1:]

        parts = path.split(".")
        self._recursive_strip_path(data, parts)

    def _recursive_strip_path(self, data: Any, parts: List[str]) -> None:
        if not parts:
            return

        key = parts[0]
        if len(parts) == 1:
            if isinstance(data, dict):
                data.pop(key, None)
            elif isinstance(data, list):
                for item in data:
                    if isinstance(item, dict):
                        item.pop(key, None)
            return

        # More parts remaining
        if isinstance(data, dict):
            if key in data:
                self._recursive_strip_path(data[key], parts[1:])
        elif isinstance(data, list):
            for item in data:
                if isinstance(item, dict) and key in item:
                    self._recursive_strip_path(item[key], parts[1:])

    # High-Level Wrappers
    def initialize(self, args: InitializeArguments) -> RequestFuture:
        assert self._writer is not None
        writer = self._writer
        return self._send_wrapper(
            "initialize",
            lambda: self.client.initialize(writer, args),
        )

    def launch(self, args: LaunchArguments) -> RequestFuture:
        assert self._writer is not None
        writer = self._writer
        return self._send_wrapper(
            "launch", lambda: self.client.launch(writer, args)
        )

    def evaluate(self, args: EvaluateArguments) -> RequestFuture:
        assert self._writer is not None
        writer = self._writer
        return self._send_wrapper(
            "evaluate", lambda: self.client.evaluate(writer, args)
        )

    def threads(self) -> RequestFuture:
        assert self._writer is not None
        writer = self._writer
        return self._send_wrapper(
            "threads", lambda: self.client.threads(writer)
        )

    def stack_trace(self, args: StackTraceArguments) -> RequestFuture:
        assert self._writer is not None
        writer = self._writer
        return self._send_wrapper(
            "stackTrace", lambda: self.client.stack_trace(writer, args)
        )

    async def verify_all_expectations(self) -> None:
        """Awaits all pending futures and verifies event expectations."""
        # 1. Check if pending expectations are already satisfied by unmatched history
        self._check_event_expectations_against_history()

        # 2. Verify request futures (Fail-fast before waiting for events)
        for fut in self.pending_futures:
            try:
                await fut
            except Exception as e:
                print(f"Expectation failure in background request: {e}")
                raise
        self.pending_futures.clear()

        # 3. If there are still pending expectations, wait for up to 2 seconds for them to arrive
        if self.event_expectations:
            print(
                f"Pending event expectations remain. Waiting up to 2 seconds for them to arrive..."
            )
            try:
                async with asyncio.timeout(2.0):
                    futs = [exp.fut for exp in self.event_expectations]
                    await asyncio.gather(*futs)
            except asyncio.TimeoutError:
                print("Timed out waiting for pending event expectations.")

        # 4. Verify event expectations
        if self.event_expectations:
            raise AssertionError(
                f"Pending event expectations were not met: {[e.event_name for e in self.event_expectations]}"
            )

    def _check_event_expectations_against_history(self) -> None:
        """Helper to check pending event expectations against unmatched history."""
        for exp in list(self.event_expectations):
            for msg in list(self.unmatched_events):
                if (
                    msg.get("type") == "event"
                    and msg.get("event") == exp.event_name
                ):
                    all_passed = True
                    for check in exp.checks:
                        if not self._verify_partial(
                            msg, check, raise_error=False
                        ):
                            all_passed = False
                            break
                    if all_passed:
                        if not exp.fut.done():
                            exp.fut.set_result(msg)
                        self.event_expectations.remove(exp)
                        self.unmatched_events.remove(msg)
                        break

    async def teardown(self) -> None:
        """Cleans up connections and processes."""
        for task in self._request_tasks:
            if not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
        self._request_tasks.clear()

        if self._client_task:
            self._client_task.cancel()
            try:
                await self._client_task
            except asyncio.CancelledError:
                pass
        if self._process_task:
            self._process_task.cancel()
            try:
                await self._process_task
            except asyncio.CancelledError:
                pass
        if self._writer:
            self._writer.close()
            await self._writer.wait_closed()
        if self.proc:
            try:
                self.proc.terminate()
            except ProcessLookupError:
                pass
        if self._server_log_task:
            try:
                await asyncio.wait_for(self._server_log_task, timeout=5.0)
            except TimeoutError:
                print(
                    "Teardown: Timeout waiting for server log reader to finish, cancelling..."
                )
                self._server_log_task.cancel()
                try:
                    await self._server_log_task
                except asyncio.CancelledError:
                    pass
            except Exception as e:
                print(f"Teardown: error draining server process: {e}")

        if self._isolate_dir:
            self._isolate_dir.cleanup()
            self._isolate_dir = None

    async def _read_server_log(self) -> None:
        if self.proc is None:
            raise RuntimeError("Cannot read server log: process is not started")
        try:
            async for event in self.proc:
                if isinstance(event, StdoutEvent):
                    self._server_logs.append(
                        f"[zxdb stdout] {event.text.decode('utf-8', errors='replace')}"
                    )
                elif isinstance(event, StderrEvent):
                    self._server_logs.append(
                        f"[zxdb stderr] {event.text.decode('utf-8', errors='replace')}"
                    )
                elif isinstance(event, TerminationEvent):
                    self._server_logs.append(
                        f"[zxdb terminated] exit code: {event.return_code}\n"
                    )
        except asyncio.CancelledError:
            pass
        except Exception as e:
            self._server_logs.append(f"Error reading server log: {e}\n")
            raise

    def dump_server_logs(self) -> None:
        """Dumps captured DAP server stdout/stderr logs to host output."""
        if not self._server_logs:
            return
        print("\n--- Captured DAP Server Logs ---", flush=True)
        for line in self._server_logs:
            print(line, end="", flush=True)
        print("---------------------------------\n", flush=True)


class DapTestCase(unittest.IsolatedAsyncioTestCase):
    """Base class for DAP integration tests, handling server lifecycle fixtures."""

    async def asyncSetUp(self) -> None:
        print(f"\n[TEST START] {self.id()}", flush=True)
        self.framework = DapTestFramework()
        self.port = portpicker.pick_unused_port()

        # Start server and connect
        await self.framework.start_server(self.port)

    async def asyncTearDown(self) -> None:
        test_failed = False
        outcome = getattr(self, "_outcome", None)
        if outcome:
            errors = getattr(outcome, "errors", [])
            for _, exc_info in errors:
                if exc_info:
                    test_failed = True
                    break

        try:
            await self.framework.verify_all_expectations()
        except Exception as e:
            test_failed = True
            print(f"Teardown: Expectations failed: {e}", flush=True)
            raise
        finally:
            # Disconnect and terminate server (drains logs)
            await self.framework.teardown()
            if test_failed:
                self.framework.dump_server_logs()
            print(f"[TEST END] {self.id()}\n", flush=True)

    # Delegation methods for cleaner test syntax
    def initialize(self, args: InitializeArguments) -> RequestFuture:
        return self.framework.initialize(args)

    def launch(self, args: LaunchArguments) -> RequestFuture:
        return self.framework.launch(args)

    def evaluate(self, args: EvaluateArguments) -> RequestFuture:
        return self.framework.evaluate(args)

    def threads(self) -> RequestFuture:
        return self.framework.threads()

    def stack_trace(self, args: StackTraceArguments) -> RequestFuture:
        return self.framework.stack_trace(args)

    def split_request(self, seq: int, delay: float = 0.1) -> None:
        self.framework.split_request(seq, delay)

    def on_event(self, event_name: str) -> EventFuture:
        return self.framework.on_event(event_name)

    def evaluate_and_compare(
        self, golden_file: str, ignore_list: Optional[List[str]] = None
    ) -> None:
        self.framework.evaluate_and_compare(golden_file, ignore_list)

    def evaluate_and_save(
        self, golden_file: str, ignore_list: Optional[List[str]] = None
    ) -> None:
        self.framework.evaluate_and_save(golden_file, ignore_list)

    async def verify_all_expectations(self) -> None:
        await self.framework.verify_all_expectations()
