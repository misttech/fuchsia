<!--* freshness: { reviewed: '2026-10-12' } *-->

# Zxdb Client Library (`zxdb/client`)

`zxdb/client` is the UI-independent library for building debugger frontends. It
connects to a target `debug_agent`, models running processes, threads, and stack
frames, and exposes APIs to inspect and control target execution.

## Main Objects

The client object model orders classes from largest to smallest scope.

### Session

`Session` is the top-level connection object. It manages the connection to the
remote system (`Connect`, `Disconnect`, `OpenMinidump`) and dispatches incoming
`debug_ipc` notifications.

To initialize the debugger client, create a `Session` on a thread with an active
`debug::MessageLoop` (`//src/developer/debug/shared`). `Session` owns the
`RemoteAPI` instance, which unit tests often mock via `MockRemoteAPI`.

### System

`System` represents global target OS state on the client. It owns the list of
`Target`, `Breakpoint`, `Filter`, and `SymbolServer` objects. `Session` and
`System` have a 1:1 relationship (`Session` owns connection state, while
`System` owns target OS state).

### Target

`Target` is a container for a debuggable process and its configuration (such as
command-line arguments) across process runs. A `Target` may or may not have a
running process at any moment, and `System` always keeps at least one `Target`.
When the process runs, `Target::GetProcess()` returns its active `Process`
object.

### Process

`Process` represents a live attached or running process on the target. It owns
the running `Thread` objects and exposes APIs to read and write memory or pause
and continue all threads.

In the console UI, the `process` noun refers to a `Target` (which can be idle or
running), whereas `client::Process` always represents a live process.

### Thread

`Thread` represents a live thread inside a `Process`. It owns a `Stack` and
drives thread stepping through `ThreadController` instances. `Thread` populates
its `Stack` when it stops on an exception, though initial process or thread
start exceptions may have an empty stack.

### Stack

`Stack` manages the backtrace of a stopped `Thread`, including physical stack
frames and synthetic inline function frames.

When a thread stops on an exception, `debug_agent` sends only the top 1 to 2
stack frames so internal stepping controllers avoid full stack unwinds. Callers
that need the full backtrace must call `Stack::SyncFrames` asynchronously.

### Frame

`Frame` represents a single stack frame (either a physical frame or an inline
function call). Callers evaluate expressions and inspect local variables in the
context of a frame through `Frame::GetEvalContext()`.

## Other Objects

### Breakpoint

`System` owns all `Breakpoint` objects globally. A breakpoint can scope itself
to a specific `Target` or `Thread`, but the `Breakpoint` instance itself remains
owned by `System`.

### Filter

`Filter` watches for matching process names, job KOIDs, or component
monikers/URLs and attaches automatically when a match starts.

## Thread Control

`ThreadController` subclasses implement thread stepping policies. To step a
thread, construct the desired `ThreadController` subclass and pass it to
`Thread::ContinueWith`. The controller inspects stop exceptions and continues or
stops the thread:

- `FinishThreadController`: Runs until the current stack frame returns.
- `StepOverThreadController`: Steps over function calls on the current line.
- `StepThreadController`: Steps into source lines or machine instructions.
- `UntilThreadController`: Runs until execution reaches a target address.

`Thread` nests controllers on a stack so a thread can handle nested steps and
still stop when the outer step-over finishes.

## Expressions

`zxdb/expr` evaluates expressions and variables using an `EvalContext`. Callers
normally get this `EvalContext` from `Frame::GetEvalContext()`, which provides
access to local variables, registers, and process memory.

## Interface

Public headers expose these client classes and observers:

- `zxdb::ClientObject`: Base class for all client objects holding a pointer to
  the owning `Session` (`client_object.h`).
- `zxdb::Session`, `zxdb::SessionConnectionInfo`, `zxdb::SessionConnectionType`:
  Top-level connection manager (`Connect`, `Disconnect`, `OpenMinidump`,
  `OnStreamReadable`) owning `System`, `ArchInfo`, and `RemoteAPI`
  (`session.h`).
- `zxdb::RemoteAPI`, `zxdb::MinidumpRemoteAPI`: Asynchronous `debug_ipc`
  request/reply interface and its Crashpad minidump implementation
  (`remote_api.h`, `minidump_remote_api.h`).
- `zxdb::System`, `zxdb::SystemObserver`: Global system state manager for
  `Target`, `Breakpoint`, `Filter`, `SymbolServer`, `SystemSymbols`, and
  `DownloadManager` (`Pause`, `Continue`, `CancelAllThreadControllers`)
  (`system.h`, `system_observer.h`).
- `zxdb::Target`, `zxdb::TargetObserver`: Holder for a debuggable process and
  its configuration (`State::kNone`, `kStarting`, `kAttaching`, `kRunning`,
  `Launch`, `Kill`, `Attach`, `Detach`, `GetProcess`, `GetSymbols`) (`target.h`,
  `target_observer.h`).
- `zxdb::Process`, `zxdb::ProcessObserver`: Live process model (`GetTarget`,
  `GetKoid`, `GetName`, `GetComponentInfo`, `GetSymbols`, `GetModules`,
  `GetAspace`, `GetThreads`, `SyncThreads`, `Pause`, `Continue`,
  `ContinueUntil`, `ReadMemory`, `WriteMemory`, `LoadInfoHandleTable`,
  `GetAsyncTaskProvidersForLanguage`) (`process.h`).
- `zxdb::Thread`, `zxdb::ThreadObserver`: Live thread model (`GetProcess`,
  `GetKoid`, `GetState`, `GetBlockedReason`, `CurrentStopInfo`, `Pause`,
  `Continue`, `ContinueWith`, `AddController`, `AddPostStopTask`,
  `CancelAllThreadControllers`, `ResumeFromAsyncThreadController`, `JumpTo`,
  `StepInstructions`, `GetStack`, `GetAsyncTaskTree`) (`thread.h`).
- `zxdb::Stack`, `zxdb::Stack::Delegate`: Stopped thread backtrace manager for
  frame synchronization (`SyncFrames`), inline frame expansion, and
  `FrameFingerprint` computation (`stack.h`).
- `zxdb::Frame`: Physical or inline stack frame interface (`GetThread`,
  `IsInline`, `GetPhysicalFrame`, `GetLocation`, `GetAddress`, `GetTrust`,
  `GetRegisterCategorySync`, `GetRegisterCategoryAsync`, `WriteRegister`,
  `GetSymbolDataProvider`, `GetEvalContext`, `GetSourceFileProvider`,
  `GetLocalVariables`) (`frame.h`).
- `zxdb::AsyncTask`, `zxdb::AsyncTaskProvider`, `zxdb::AsyncTaskTree`,
  `zxdb::FuchsiaAsyncRustTaskProvider`: Interfaces and tree container for
  inspecting async tasks (such as Rust futures) on a `Thread` (`async_task.h`,
  `async_task_tree.h`).
- `zxdb::Breakpoint`, `zxdb::BreakpointSettings`, `zxdb::BreakpointLocation`,
  `zxdb::BreakpointObserver`, `zxdb::ExecutionScope`: Global and scoped
  breakpoints with per-process `BreakpointLocation` controls (`breakpoint.h`,
  `breakpoint_settings.h`, `breakpoint_location.h`).
- `zxdb::Filter`: Process, component, and job attach filter synchronized with
  `debug_agent` (`filter.h`).
- `zxdb::ThreadController`, `zxdb::StepThreadController`,
  `zxdb::StepIntoThreadController`, `zxdb::StepOverThreadController`,
  `zxdb::FinishThreadController`, `zxdb::FinishPhysicalFrameThreadController`,
  `zxdb::UntilThreadController`, `zxdb::UntilProcessController`,
  `zxdb::CallFunctionThreadController`: Composable controllers for thread
  stepping, finishing, running until addresses, and calling target functions
  (`thread_controller.h`).
- `zxdb::SettingStore`, `zxdb::MapSettingStore`, `zxdb::SettingSchema`,
  `zxdb::SettingValue`, `zxdb::SettingStoreObserver`: Schema-validated
  hierarchical configuration store for `System`, `Target`, `Thread`,
  `Breakpoint`, and `Filter` (`setting_store.h`).
- `zxdb::MemoryDump`, `zxdb::Disassembler`, `zxdb::SymbolServer`,
  `zxdb::ArchInfo`, `zxdb::PrettyStackManager`: Memory dump container, LLVM
  instruction disassembler, symbol server client, architecture descriptor, and
  frame globbing manager.

## Invariants

Callers and implementations must preserve these invariants:

- `Session` and `System` maintain a 1:1 lifecycle correspondence, and `Session`
  owns the `System` instance.
- `System` always contains at least one `Target`. `System::DeleteTarget` fails
  if the target is running or is the only remaining `Target` in `System`.
- A `Target` persists across process runs (`State::kNone`, `kStarting`,
  `kAttaching`, `kRunning`). A `Process` object exists only while a process is
  active, and `Process::GetTarget()` is never null.
- Containers own the raw pointers returned by `System::GetTargets`,
  `System::GetBreakpoints`, `System::GetFilters`, `System::GetSymbolServers`,
  `Process::GetThreads`, and `Breakpoint::GetLocations`. Never cache those raw
  pointers across message loop turns. Use `fxl::WeakPtr` instead.
- On thread exception stops, `debug_agent` sends only the top 1 to 2 stack
  frames. Call `Stack::SyncFrames` asynchronously for full backtraces, and never
  assume `Stack` is non-empty on an exception stop.
- `Frame::GetRegisterCategorySync` guarantees synchronous availability only for
  general registers. Call `Frame::GetRegisterCategoryAsync` on the top frame for
  other register categories, and call `Frame::WriteRegister` only on the top
  physical frame.
- Active `ThreadController` instances evaluate thread stops via `OnThreadStop`
  returning `StopOp`. `kFuture` takes highest precedence (waiting for
  `Thread::ResumeFromAsyncThreadController`), `kStopDone` beats `kContinue`, and
  the thread stops if all controllers return `kUnexpected`.
- `Process` bounds `stdout_` and `stderr_` capture buffers to
  `Process::kMaxIOBufferSize` (1 MiB) using circular deques.

## Side Effects

`zxdb/client` produces these IPC, file I/O, and target side effects:

- Serializes `debug_ipc` messages over `debug::StreamBuffer` and
  `debug::BufferedFD` to talk to the remote `debug_agent`.
- Mutates remote target state over IPC by starting, attaching, detaching, and
  killing processes, and by suspending or resuming threads. Also writes memory
  (`Process::WriteMemory`), writes CPU registers (`Frame::WriteRegister`,
  `Thread::JumpTo`), sets breakpoints, and calls target functions
  (`CallFunctionThreadController`).
- Reads local Crashpad minidumps (`MinidumpRemoteAPI`), reads local symbols and
  source files (`SourceFileProviderImpl`), and downloads debug symbols
  (`DownloadManager`, `SymbolServer`).
- Posts asynchronous tasks to `debug::MessageLoop` and notifies `Session`,
  `System`, `Target`, `Process`, `Thread`, `Breakpoint`, `Download`,
  `Component`, and `SettingStore` observers.
- Emits debugger usage telemetry events through `AnalyticsReporter`.

## Verification

Run these commands to build and test `zxdb/client`:

- Build: `fx build //src/developer/debug/zxdb/client`
- Test: `fx test zxdb_tests`
