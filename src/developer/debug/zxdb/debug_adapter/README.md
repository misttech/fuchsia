<!--* freshness: { reviewed: '2026-10-12' } *-->

# Debug Adapter (`zxdb/debug_adapter`)

`//src/developer/debug/zxdb/debug_adapter` implements a Debug Adapter Protocol
(DAP) server (Layer 6) using the `cppdap` library to support IDE debugging. Read
the [DAP specification](https://microsoft.github.io/debug-adapter-protocol) for
protocol details.

## Usage

Follow these steps to connect VS Code to `zxdb`:

1.  Install the Fuchsia VS Code extension.
2.  Open **Run > Start Debugging** and select the `zxdb` configuration.
3.  Enter the target executable or component filter to attach or start.

## Submodules

The debug adapter consists of two core classes:

- `DebugAdapterServer`: Manages the TCP connection to the DAP client. The server
  accepts one client at a time. When the client disconnects, the server resets
  the connection and listens for a new client.
- `DebugAdapterContext`: Parses incoming DAP requests using `cppdap` and
  executes them against the shared `zxdb` client `Session`.

The diagram below shows how the debug adapter interacts with other `zxdb`
modules at runtime:

![Runtime view](runtime_view.png)

`Console` and `DebugAdapterContext` share the same `Session` instance, so users
can control a debugging session from both the IDE and the `zxdb` console.

## Interactions with the `zxdb` Client Session

`DebugAdapterContext` drives the `zxdb` client `Session` (`Target`, `Process`,
and `Thread`). When handling `OnRequestAttach`, `DebugAdapterContext` reuses an
idle `Target` (`Target::State::kNone`) or calls `System::CreateNewTarget`, and
manages connection-scoped `Filter` instances.

## Interface

Public headers and subdirectories expose these DAP server and context types:

- `zxdb::DebugAdapterServer`, `zxdb::DebugAdapterServerObserver`: TCP server and
  connection lifecycle observer (`server.h`).
- `zxdb::DebugAdapterContext`: Per-connection DAP session controller, observer,
  and frame/variable ID table manager (`context.h`).
- `zxdb::DebugAdapterReader`, `zxdb::DebugAdapterWriter`: `cppdap::Reader` and
  `cppdap::Writer` adapters over `debug::StreamBuffer` (`context.h`).
- `zxdb::VariablesType`, `zxdb::VariablesRecord`: Scope categories (`kLocal`,
  `kArguments`, `kRegister`, `kChildVariable`) and variable reference table
  entries (`context.h`).
- `zxdb::DataId`: DAP `dataId` identifier parser and validator (`data_id.h`).
- `zxdb::AsyncBacktraceSubscription`, `zxdb::FormatAsyncTaskTree`,
  `dap::AsyncTaskNode`, `dap::AsyncBacktraceUpdate`: Subscription manager and
  DAP types for streaming async task trees (`async_backtrace_subscription.h`).
- [handlers](./handlers/README.md): DAP request handlers and custom `zxdb`
  protocol extensions for target lifecycle, stepping, breakpoints, backtraces,
  variables, and REPL evaluation.

## Invariants

Callers and implementations must preserve these invariants:

- Call all methods on `DebugAdapterContext` and `DebugAdapterServer` (except
  `DebugAdapterServer::ListenBackgroundThread`) on the main `MessageLoop`
  thread.
- `Console` must outlive `DebugAdapterServer`.
- `DebugAdapterContext` registers DAP handlers, session observers, and
  `AsyncBacktraceSubscription` only after sending `InitializeResponse`. It
  defers `InitializeResponse` until any pending `Session` connection finishes.
- `DebugAdapterContext::OnStreamReadable` calls `HasCompleteMessage()` before
  reading a `cppdap` payload. This check ensures a full `Content-Length:` header
  and body exist in the non-blocking `StreamBuffer`.
- Frame ID `0` and `VariablesRecord` ID `0` are invalid. Valid IDs are
  monotonically increasing positive integers.
- `DebugAdapterContext` preserves frame IDs for a thread across stack updates.
  When a stack shrinks, `DebugAdapterContext` deletes out-of-bounds frame IDs
  and their associated variable IDs.
- `AsyncBacktraceSubscription` syncs and emits non-empty async task trees only
  when all threads in the process stop and the thread has stack frames.
- `DebugAdapterContext` tracks and deletes only the breakpoints and filters that
  it created, preserving user console breakpoints and startup filters.

## Side Effects

`zxdb/debug_adapter` produces these socket, thread, and session side effects:

- `DebugAdapterServer` opens an IPv6 TCP listening socket, spawns a background
  thread on `select()`/`accept()`, and resumes listening after client
  disconnect.
- Receiving a DAP `DisconnectRequest` runs the destroy connection callback,
  which quits the `zxdb` `Console` (`console()->Quit()`).
- `DebugAdapterContext` registers itself and `AsyncBacktraceSubscription` as
  observers on `Session`, `Thread`, `Process`, `Breakpoint`, and `System`. On
  destruction, it deletes all DAP-created breakpoints and filters.
- Emits standard and custom DAP events (`InitializedEvent`, `ThreadEventZxdb`,
  `StoppedEvent`, `ProcessEvent`, `ExitedEvent`, `TerminatedEvent`,
  `BreakpointEvent`, `InvalidatedEvent`, `AsyncBacktraceUpdate`) over the client
  stream.
- `GetElidedFrames` sets a `Process` kind to `Process::Kind::kTest` when
  `TestFailureStackMatcher` matches frames on the stack.
- Reports `ConsoleType::Type::kDebugAdapter` to `zxdb` session analytics on
  initialization.

## Verification

Run these commands to build and test `zxdb/debug_adapter`:

- Build: `fx build //src/developer/debug/zxdb/debug_adapter`
- Test: `fx test zxdb_tests`
