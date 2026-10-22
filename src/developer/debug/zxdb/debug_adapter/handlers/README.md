<!--* freshness: { reviewed: '2026-10-20' } *-->

# Zxdb Debug Adapter Request Handlers (`zxdb/debug_adapter/handlers`)

## Overview

`//src/developer/debug/zxdb/debug_adapter/handlers` implements Debug Adapter
Protocol (DAP) request handlers and custom `zxdb` extensions. These handlers
manage target attach and launch, thread stepping, breakpoints and watchpoints,
stack and async backtraces, variable scopes, expression evaluation, and raw
console command execution.

## Interface

Public headers group DAP request records and handlers into these areas:

- Target attach, launch, and process queries (`request_attach.h`,
  `request_launch.h`, `request_process.h`, `request_terminate.h`,
  `request_zxdb_detach.h`): `dap::AttachRequestZxdb`,
  `zxdb::OnRequestAttach(DebugAdapterContext*, const dap::AttachRequestZxdb&)`,
  `dap::LaunchRequestZxdb`, `zxdb::SplitDapCommand(const dap::string&,
  dap::array<dap::string>&)`, `zxdb::OnRequestLaunch(DebugAdapterContext*, const
  dap::LaunchRequestZxdb&)`, `dap::ZxdbProcessInfo`, `dap::ZxdbProcessRequest`,
  `dap::ZxdbProcessResponse`, and
  `zxdb::OnRequestZxdbProcess(DebugAdapterContext*, const
  dap::ZxdbProcessRequest&)`.
- Execution control and thread stepping (`request_continue.h`,
  `request_pause.h`, `request_next.h`, `request_step_in.h`,
  `request_step_out.h`, `request_threads.h`):
  `zxdb::OnRequestContinue(DebugAdapterContext*, const dap::ContinueRequest&)`,
  `dap::PauseRequestZxdb`, `dap::PauseResponseZxdb`,
  `dap::ProcessStoppedEventZxdb`, and `zxdb::OnRequestPause`. Also exposes
  `zxdb::OnRequestNext`, `zxdb::OnRequestStepIn`, `zxdb::OnRequestStepOut`,
  `dap::ThreadZxdb`, `dap::ThreadEventZxdb`, `dap::ThreadsResponseZxdb`, and
  `zxdb::OnRequestThreads`.
- Source, function, and data breakpoints (`request_breakpoint.h`,
  `request_function_breakpoint.h`, `request_data_breakpoint.h`):
  `zxdb::OnRequestBreakpoint`, `zxdb::OnRequestFunctionBreakpoint`,
  `zxdb::OnRequestDataBreakpointInfo`, and `zxdb::OnRequestSetDataBreakpoints`.
- Stack traces and async backtraces (`request_stacktrace.h`,
  `request_async_backtrace.h`): `dap::StackTraceRequestZxdb`,
  `zxdb::OnRequestStackTrace`, `dap::ZxdbAsyncBacktraceRequest`,
  `dap::ZxdbAsyncBacktraceResponse`, and `zxdb::OnRequestZxdbAsyncBacktrace`.
- Scopes, variables, expression evaluation, and console command execution
  (`request_scopes.h`, `request_variables.h`, `request_evaluate.h`,
  `request_console_command.h`): `zxdb::OnRequestScopes`,
  `zxdb::OnRequestVariables`, `zxdb::OnRequestEvaluate`,
  `dap::ZxdbConsoleCommandRequest`, `dap::ZxdbConsoleCommandResponse`, and
  `zxdb::OnRequestZxdbConsoleCommand`.

## Invariants

Handler implementations must preserve these invariants:

- `OnRequestAttach` requires a non-empty process pattern. It parses decimal
  strings as `uint64_t` KOIDs for direct attach and resolves symbolic patterns
  via `debug_ipc::ResolveFilterType`.
- Without client `RunInTerminal` support, `OnRequestLaunch` requires
  `req.process` to contain `://` and end with `.cm`.
- `OnRequestBreakpoint` requires `req.source.path` to be present, absolute, and
  canonicalizable via `std::filesystem::weakly_canonical`.
- `OnRequestDataBreakpointInfo` and `OnRequestSetDataBreakpoints` format
  `DataId` as `<process_koid>:0x<address_hex>:<byte_size>` with `process_koid >
  0` and `address <= UINT64_MAX - byte_size`. The watched value must reside in
  memory (not registers or bitfields), and `byte_size` must pass
  `BreakpointSettings::ValidateSize`. Access must be `write` or `readWrite`, and
  `hitCondition` must be empty.
- Stepping (`OnRequestNext`, `OnRequestStepIn`, `OnRequestStepOut`), variable
  queries (`OnRequestScopes`, `OnRequestVariables`), and
  `OnRequestDataBreakpointInfo` require a stopped thread via
  `DebugAdapterContext::CheckStoppedThread`.
- `OnRequestVariables` requires `VariableResponseContext` to finish all pending
  evaluations (`FX_CHECK(!pending_evals)`) before calling the response callback,
  and requires a non-null `Frame` for every valid `VariablesRecord`.
- `OnRequestZxdbAsyncBacktrace` requires `threadId > 0` and requires
  `process->AllThreadsStopped()` to return `true`.
- `OnRequestPause` rejects requests that set both `processId` and `threadId`,
  and requires `processId > 0` when present.
- `OnRequestZxdbProcess` requires `req.pid > 0` when present.
- `OnRequestZxdbConsoleCommand` validates `req.frameId` via
  `DebugAdapterContext::FrameforId` when present, and returns a DAP error if
  `OfflineCommandContext` reports any command errors.
- `OnRequestEvaluate` accepts only requests where `req.context` equals `repl`.
- Per the DAP specification, handlers must send the command `Response` (such as
  `PauseResponse`) **before** any resulting `Event` (such as `StoppedEvent` or
  `ProcessStoppedEventZxdb`). Never emit events inside `debug_ipc` reply
  callbacks before the request handler sends its `Response`.
- New and refactored DAP handlers must call the `zxdb::client` object model
  directly instead of building `Console` command strings or reading stateful
  `ConsoleContext` selections.
- Use custom `*Zxdb` request types (such as `TerminateRequestZxdb` or
  `StackTraceRequestZxdb`) only when a request needs custom fields. Declare all
  extension fields as `std::optional<T>` with explicit defaults for standard DAP
  clients.

## Side Effects

Executing DAP request handlers produces these session and IPC side effects:

- Mutates `zxdb::System` state by creating and attaching `Target` instances,
  updating `Filter` and `Breakpoint` objects, and pausing or continuing
  `System`, `Process`, or `Thread` execution.
- Mutates `DebugAdapterContext` state by storing and deleting connection-scoped
  filters, breakpoints, frame IDs, and variable records, and by running
  `OnProcessPaused` and `OnThreadPaused` callbacks.
- Sends asynchronous DAP requests and events via
  `DebugAdapterContext::dap().send()`, including `dap::RunInTerminalRequest`,
  `dap::StoppedEvent`, and `dap::ProcessStoppedEventZxdb`.
- Sends `RemoteAPI::RunComponent` IPC requests when launching components without
  `RunInTerminal` support.
- Updates `ConsoleContext` active selections and runs console commands via
  `Console::ProcessInputLine` during `OnRequestZxdbConsoleCommand` and `repl`
  `OnRequestEvaluate` calls.
- Triggers asynchronous stack syncing (`Stack::EnsureFrames`), async task tree
  syncing (`AsyncTaskTree::Sync`), and expression evaluation (`EvalExpression`,
  `FillFormatNodeValue`, `FillFormatNodeDescription`).

## Verification

Run these commands to build and test `zxdb/debug_adapter/handlers`:

- Build: `fx build //src/developer/debug/zxdb/debug_adapter`
- Test: `fx test zxdb_tests`
