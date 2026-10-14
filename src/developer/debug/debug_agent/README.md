<!--* freshness: { reviewed: '2026-10-12' } *-->

# Debug Agent (`debug_agent`)

`debug_agent` is the target-side backend stub for the Fuchsia debugger. It runs
on the target Fuchsia system (and experimentally on Linux hosts). It
communicates over `debug_ipc` ([../ipc](../ipc/README.md)) with the `zxdb`
frontend ([../zxdb](../zxdb/README.md)) on the developer's workstation.

## User Documentation

Read the debugger [setup](../../../../docs/development/debugger/running.md) and
[usage](../../../../docs/development/debugger/README.md) guides for user-facing
workflows.

## Manual Testing

Test these four workflows whenever you edit process attachment or component
starting:

- `run-component fuchsia-pkg://fuchsia.com/debug_agent#meta/debug_agent.cm` (or
  another component URL).
- `attach some_filter` when a matching process already exists.
- `attach some_filter` before a matching process starts.
- `attach 56421` to attach by numeric PID/KOID (`debug_agent` uses separate code
  paths for PID and filter attachment).

## Interface

Public headers expose these core classes and helpers:

- `debug_agent::DebugAgent` / `debug_agent::DebugAgentOptions`: Core backend
  state machine managing attached jobs, processes, breakpoints, filters, and
  `RemoteAPI` dispatch (`debug_agent.h`).
- `debug_agent::DebugAgentServer`: `fuchsia.debugger.DebugAgent` FIDL server
  that bridges FIDL requests and `FilterIterator` attachment into `DebugAgent`
  (`debug_agent_server.h`).
- `debug_agent::DebuggedJob` / `debug_agent::DebuggedJobCreateInfo`: Job
  exception channel observer that intercepts newly starting processes
  (`debugged_job.h`).
- `debug_agent::DebuggedProcess` / `debug_agent::DebuggedProcessCreateInfo`:
  Per-process state controller managing threads, memory access, module
  notifications, `stdout`/`stderr` capture, and serialized breakpoint step-over
  queues (`debugged_process.h`).
- `debug_agent::DebuggedThread`: Per-thread execution state, suspension
  ref-counting (`ClientSuspend`, `InternalSuspend`), single-stepping, and
  register/exception handling (`debugged_thread.h`).
- `debug_agent::Breakpoint` / `debug_agent::ProcessBreakpoint`: Logical
  (`Breakpoint`) and per-process address/range (`ProcessBreakpoint`) breakpoint
  abstractions (`breakpoint.h`, `process_breakpoint.h`).
- `debug_agent::SoftwareBreakpoint`, `debug_agent::HardwareBreakpoint`,
  `debug_agent::Watchpoint`: Concrete `ProcessBreakpoint` subclasses for
  instruction-patching traps, CPU debug register breakpoints, and memory
  read/write watchpoints (`software_breakpoint.h`, `hardware_breakpoint.h`,
  `watchpoint.h`).
- `debug_agent::RemoteAPI` / `debug_agent::RemoteAPIAdapter`: Abstract RPC
  handler interface for `debug_ipc` requests and stream dispatch adapter
  (`remote_api.h`, `remote_api_adapter.h`).
- `debug_agent::SystemInterface` / `debug_agent::ZirconSystemInterface`:
  Abstract and Zircon-backed representations of the target job tree, component
  manager, and limbo provider (`system_interface.h`,
  `zircon_system_interface.h`).
- `debug_agent::ComponentManager`: Component lifecycle event listener, test
  runner (`fuchsia.test.manager`), and component matcher
  (`component_manager.h`).
- `debug_agent::LimboProvider`: Interface to `ProcessLimbo` for inspecting,
  claiming, and releasing crashed processes waiting in limbo
  (`limbo_provider.h`).
- `debug_agent::ExceptionHandle`: Wrapper around a kernel exception token and
  its resolution state (`exception_handle.h`).
- `debug_agent::Filter`: Job, process name, and component moniker/URL filter
  matcher (`filter.h`).
- `debug_agent::UnwindStack`: Stack unwinding wrapper around
  `//src/lib/unwinder` (`unwind.h`).
- `debug_agent::AutomationHandler`: Executor for automated memory and register
  readout instructions attached to breakpoint hits (`automation_handler.h`).
- [posix](./posix/README.md): Allocation-free POSIX/Linux system call helpers
  (`HANDLE_EINTR`, `IGNORE_EINTR`, `linux::DirReaderLinux`,
  `posix::ShuffleFileDescriptors`).

## Invariants

Callers and implementations must preserve these invariants:

- Initialize a `debug::PlatformMessageLoop` (`debug::MessageLoopFuchsia` on
  Fuchsia) on the current thread before constructing `DebugAgent`. The
  `debug::StreamBuffer` passed to `DebugAgent::Connect` or `RemoteAPIAdapter`
  must outlive the adapter.
- `DebugAgent::SendNotification` requires `ipc_version_ > 0` (negotiated via the
  IPC `Hello` handshake) before serializing notifications to a stream.
- Call `Init()` and check for an OK `debug::Status` before using any
  `DebuggedJob` or `DebuggedProcess` instance.
- A `DebuggedThread` runs only when neither `client_suspend_handle_` nor any
  internal `SuspendHandle` token exists.
- `DebuggedProcess` serializes breakpoint step-overs through `step_over_queue_`
  so only one `ProcessBreakpoint` steps over at a time. `DebuggedProcess`
  ignores duplicate step-over requests for the same thread and breakpoint.
- `SoftwareBreakpoint::StepOverCleanup` must run only after the next queued
  breakpoint's `ExecuteStepOver` acquires its suspend tokens. It uses a
  per-thread multimap (`suspend_tokens_`) to prevent premature thread resumption
  between consecutive step-overs.
- Processes claimed from `LimboProvider` lack `ZX_RIGHT_DESTROY`. When killing a
  `from_limbo` process fails with `ZX_ERR_ACCESS_DENIED`, `DebugAgent` detaches,
  records its KOID in `killed_limbo_procs_`, and calls
  `LimboProvider::ReleaseProcess` once it re-enters limbo.
- `ProcessBreakpoint::OnHit` does not delete one-shot breakpoints internally.
  Callers must inspect `BreakpointStats::should_delete` and delete the
  breakpoint afterward to avoid self-deletion during hit handling.
- `SystemInterface` is non-movable because `ComponentManager` holds a raw
  back-pointer to it.
- Recursive filter matching creates new `kComponentMonikerPrefix` filters only
  for moniker patterns not already present in `DebugAgent::filters_`.
- `DebuggedJob` and `AttachConfig` distinguish the exclusive `"job"` exception
  channel (crash exceptions) from the non-exclusive `"job debugger"` channel
  (`ZX_EXCP_PROCESS_STARTING`). Process attachment uses the process debugger
  exception channel instead.
- Job attachments with `kMinimal` or `kNone` priority do not claim the debugger
  exception channel and do not receive upfront `ZX_EXCP_PROCESS_STARTING` or
  `ZX_EXCP_THREAD_STARTING` notifications. Exception handlers in `DebuggedJob`
  and `DebuggedProcess` must lazily create `DebuggedProcess` and call
  `PopulateCurrentThreads` upon receiving an exception.

## Side Effects

`debug_agent` produces these target and IPC side effects:

- Reads and writes debuggee process virtual memory (including instruction
  patching in `SoftwareBreakpoint`) and thread general/debug registers
  (`HardwareBreakpoint`, `Watchpoint`, single-stepping, and unwinding).
- Binds to Zircon job and process exception channels, suspends and resumes
  threads via `SuspendHandle` tokens, and resolves or forwards kernel exceptions
  via `ExceptionHandle`.
- Exchanges messages with `zxdb` clients over `debug_ipc` streams
  (`BufferedStream` / `RemoteAPIAdapter`) and serves the
  `fuchsia.debugger.DebugAgent` FIDL protocol via `DebugAgentServer`.
- Calls Fuchsia services to start binaries, components, and tests
  (`BinaryLauncher`, `ComponentManager`), watch component lifecycle events, and
  claim or release crashed processes in `ProcessLimbo`.
- Deleting the last `DebugAgentObserver` without an active `debug_ipc` client
  quits the message loop and exits `debug_agent`.
- Captures debuggee `stdout`/`stderr` streams to emit `NotifyIO` messages to the
  client, and generates process minidumps on request.

## Verification

Run these commands to build and test `debug_agent`:

- Build: `fx build //src/developer/debug/debug_agent`
- Test: `fx test debug_agent_unit_tests.cm debug_agent_launcher_unit_tests
  debug_agent_integration_tests`
