# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for Tracing affordance."""

import abc
import os
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager

import fidl_fuchsia_tracing as f_tracing


class Tracing(abc.ABC):
    """Abstract base class for an async Tracing affordance (`dut.tracing`).

    Session Lifecycle State Machine:
        `uninitialized` -> `initialized` -> `active` -> `stopped` -> `uninitialized`

        - `uninitialized`: `is_session_initialized()` is False, `is_active()` is False.
          Calling `initialize()` transitions the session to `initialized`. Calling
          `terminate()` is a safe no-op.
        - `initialized`: `is_session_initialized()` is True, `is_active()` is False.
          Calling `start()` transitions the session to `active`. Calling `terminate()`
          cleans up and transitions back to `uninitialized`.
        - `active`: `is_session_initialized()` is True, `is_active()` is True.
          Calling `stop()` transitions the session to `stopped`. Calling `terminate()`
          aborts/closes the active session and transitions to `uninitialized`.
        - `stopped`: `is_session_initialized()` is True, `is_active()` is False.
          Calling `start()` resumes/restarts tracing (`active`), calling `terminate()`
          discards trace data and resets to `uninitialized`, and calling
          `terminate_and_download()` saves the `.fxt` trace file and resets to
          `uninitialized`.
    """

    @abc.abstractmethod
    def initialize(
        self,
        categories: list[str] | None = None,
        buffer_size: int | None = None,
        start_timeout_milliseconds: int | None = None,
        buffering_mode: f_tracing.BufferingMode | None = None,
        defer_transfer: bool | None = None,
        compression: bool | None = None,
    ) -> None:
        """Initializes a trace session.

        Args:
            categories: List of categories to trace. If `None`, defaults to the
                affordance implementation's `DEFAULT_CATEGORIES`. Any
                `"#default"` or `"default"` element in the list expands to the
                implementation's `DEFAULT_CATEGORIES` and is deduplicated with
                custom categories.
            buffer_size: Buffer size to use in MB.
            start_timeout_milliseconds: Milliseconds to wait for trace providers
                to acknowledge that they've started tracing. NB: trace providers
                that don't ACK by this deadline may still emit tracing events
                starting at some later point. Supported in `TracingUsingFc`;
                ignored in `TracingUsingFfx`.
            buffering_mode: Tells tracing providers how to buffer data:
                ONESHOT - When the buffer fills, the provider drops subsequent
                    records.
                CIRCULAR - When the buffer fills, older records are discarded to
                    make space.
                STREAMING - Data is streamed back to `trace_manager`. Providers
                    may still drop records if events are produced faster than
                    they can be streamed.
            defer_transfer: If True, `trace_manager` will delay sending data
                until tracing has stopped. Explicitly configured in
                `TracingUsingFc`; ignored in `TracingUsingFfx` (where deferred
                transfer is triggered automatically in `STREAMING` mode).
            compression: If True, compress the trace data. Defaults to `True` in
                `TracingUsingFfx` (which raises `TracingError` when `False`).
                Unsupported in `TracingUsingFc` (which logs a warning when
                `True`).

        Raises:
            TracingStateError: When trace session is already initialized.
            TracingError: On transport/FIDL communication failure or when an
                unsupported compression option is requested.
        """

    @abc.abstractmethod
    def is_active(self) -> bool:
        """Checks if there is a currently active trace.

        Returns:
            True if tracing is currently running (`active` state), False otherwise.
        """

    @abc.abstractmethod
    def is_session_initialized(self) -> bool:
        """Checks if the trace session is initialized or not.

        Returns:
            True if the session is initialized (`initialized`, `active`, or
            `stopped` state), False otherwise (`uninitialized` state).
        """

    @abc.abstractmethod
    async def start(self) -> None:
        """Starts tracing on an initialized session.

        Raises:
            TracingStateError: When trace session is not initialized or is
                already started (`active`).
            TracingError: On FFX command or FIDL communication failure.
        """

    @abc.abstractmethod
    async def stop(self, verify_trace: bool = True) -> None:
        """Stops the current active trace.

        Args:
            verify_trace: If True, verifies that the trace file is valid and
                not empty (if supported by the implementation).

        Raises:
            TracingStateError: When trace session is not initialized or not
                currently started (`active`).
            TracingError: On FFX command or FIDL communication failure.
        """

    @abc.abstractmethod
    async def terminate(self) -> None:
        """Terminates the trace session without downloading trace data and
        resets session state to `uninitialized`.

        Raises:
            TracingError: If aborting an active trace session fails.
        """

    @abc.abstractmethod
    async def terminate_and_download(
        self, directory: str, trace_file: str | None = None
    ) -> str:
        """Terminates the trace session, downloads trace data to the specified
        directory, and resets session state to `uninitialized`.

        Args:
            directory: Absolute path on the host where the trace file will be
                saved. If this directory does not exist, this method creates it.
            trace_file: Name of the output trace file. If not provided, the API
                generates a name using the
                `"trace_{device_name}_{'%Y-%m-%d-%I-%M-%S-%p'}.fxt"` format.

        Returns:
            The path to the downloaded `.fxt` trace file.

        Raises:
            ValueError: If `directory` is not a valid absolute path (enforced by
                `TracingUsingFfx` and `trace_session()`).
            TracingStateError: When trace session is not initialized or trace
                was not stopped prior to download (in `TracingUsingFc`).
            TracingError: When the method fails to stop or collect trace data.
        """

    @asynccontextmanager
    async def trace_session(
        self,
        categories: list[str] | None = None,
        buffer_size: int | None = None,
        download: bool = False,
        directory: str | None = None,
        trace_file: str | None = None,
        start_timeout_milliseconds: int | None = None,
        buffering_mode: f_tracing.BufferingMode | None = None,
        defer_transfer: bool | None = None,
        compression: bool | None = None,
        verify_trace: bool = True,
    ) -> AsyncGenerator[None, None]:
        """Starts and captures trace data within an async context and
        automatically cleans up the trace session on exit.

        If the session is not already initialized on entry, calls `initialize()`.
        Then calls `start()`, yields control to the caller, and in the `finally`
        block calls `stop()` followed by `terminate_and_download()` (when
        `download=True`) or `terminate()` (when `download=False`).

        Args:
            categories: List of categories to trace. If `None`, defaults to the
                affordance implementation's `DEFAULT_CATEGORIES`. Any
                `"#default"` or `"default"` element expands to the
                implementation's `DEFAULT_CATEGORIES`.
            buffer_size: Buffer size to use in MB.
            download: True to download trace on exit, False to skip download.
            directory: Absolute path on the host where the trace file will be
                saved. Required and must be an absolute path if `download` is
                True.
            trace_file: Name of the output trace file. Required if `download` is
                True.
            start_timeout_milliseconds: Milliseconds to wait for trace providers
                to acknowledge that they've started tracing.
            buffering_mode: Tells tracing providers how to buffer data:
                ONESHOT - When the buffer fills, the provider drops subsequent
                    records.
                CIRCULAR - When the buffer fills, older records are discarded to
                    make space.
                STREAMING - Data is streamed back to `trace_manager`. Providers
                    may still drop records if events are produced faster than
                    they can be streamed.
            defer_transfer: If True, `trace_manager` will delay sending data
                until tracing has stopped.
            compression: If True, compress the trace data.
            verify_trace: If True, verifies that the trace file is valid and
                not empty (if supported by the implementation).

        Raises:
            ValueError: If `download` is True and either `directory` is missing
                or not an absolute path, or `trace_file` is not provided.
            TracingStateError: If session state transitions are invalid.
            TracingError: If trace initialization, start, stop, or download
                fails.
        """
        if not self.is_session_initialized():
            self.initialize(
                categories=categories,
                buffer_size=buffer_size,
                start_timeout_milliseconds=start_timeout_milliseconds,
                buffering_mode=buffering_mode,
                defer_transfer=defer_transfer,
                compression=compression,
            )
        try:
            await self.start()
            yield
        finally:
            await self.stop(verify_trace=verify_trace)
            if download:
                if directory is None or not os.path.isabs(directory):
                    raise ValueError(
                        "Provide a valid absolute path to download the trace."
                    )
                if trace_file is None:
                    raise ValueError(
                        "Provide a valid trace file to download the trace."
                    )
                await self.terminate_and_download(
                    directory=directory,
                    trace_file=trace_file,
                )
            else:
                await self.terminate()
