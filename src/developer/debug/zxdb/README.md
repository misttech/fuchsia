<!--* freshness: { reviewed: '2026-10-12' } *-->

# Zxdb (`zxdb`)

`zxdb` is the host-side frontend of the Fuchsia debugger. It runs on the
developer's workstation (Linux) and talks over `debug_ipc`
([../ipc](../ipc/README.md)) to `debug_agent`
([../debug_agent](../debug_agent/README.md)) on the Fuchsia target.

## User Documentation

Read the user guide in
[//docs/development/debugger](../../../../docs/development/debugger/README.md).

## Subdirectories & Layering

Subdirectories follow a strict bottom-up dependency order (`common` <- `symbols`
<- `expr` <- `{format, client}` <- `console` <- `debug_adapter`):

- [common](./common/README.md): Layer 1 (lowest) foundational utilities (`Err`,
  `ErrOr`, `AddressRanges`, `DataExtractor`, `Curl`, `CacheDir`). Cannot depend
  on any other `zxdb` layer.
- [symbols](./symbols/README.md): Layer 2 LLVM DWARF/ELF symbol parser, indexer
  (`Index`), lazy object model (`Symbol`, `LazySymbol`, `Type`, `Function`), and
  `BuildIDIndex`.
- [expr](./expr/README.md): Layer 3 C++/Rust expression tokenizer, parser, AST
  (`ExprNode`), bytecode VM (`VmExec`), and `EvalContext` evaluation engine.
- [format](./format/README.md): Layer 4 styled output buffers (`OutputBuffer`,
  `AsyncOutputBuffer`), `FormatNode` tree rendering, and identifier/function
  formatting helpers.
- [client](./client/README.md): Layer 4 UI-independent debugger session and
  object model (`Session`, `System`, `Target`, `Process`, `Thread`, `Stack`,
  `Frame`, `Breakpoint`, `ThreadController`).
- [console](./console/README.md): Layer 5 CLI frontend, noun/verb parser,
  `ConsoleContext`, and CLI verb handlers
  ([console/commands](./console/commands/README.md)).
- [debug_adapter](./debug_adapter/README.md): Layer 6 Debug Adapter Protocol
  (DAP) server (`DebugAdapterServer`, `DebugAdapterContext`) and request
  handlers ([debug_adapter/handlers](./debug_adapter/handlers/README.md)).

## Interface

Top-level host entry points in `//src/developer/debug/zxdb` include:

- `zxdb::ConsoleMain(int argc, const char* argv[])` (`main.cc`): Host entry
  point that parses CLI flags, initializes `MessageLoopPoll` and `Session`, and
  runs the console or DAP server.
- `zxdb::ForkLocalAgent()` and `zxdb::LocalAgentResult` (`local_agent.h`,
  `local_agent.cc`): Forks a local Linux `debug_agent` over a
  `debug::BufferedBidiPipe` for `--local` on Linux x64 hosts.
- `zxdb::RunLocalAgent(fbl::unique_fd read_pipe, fbl::unique_fd write_pipe)`
  (`local_agent.cc`): Runs the child `debug_agent::DebugAgent` backed by
  `debug_agent::LinuxSystemInterface` on a `debug::MessageLoopPoll`.

## Invariants

Callers and entry points must preserve these invariants:

- `zxdb` targets build only on the host toolchain (`is_host`), and built-in
  local `debug_agent` support (`SUPPORTS_LOCAL_AGENT`) builds only on Linux x64
  (`is_linux && current_cpu == "x64"`).
- Subdirectory dependencies flow strictly upward without cycles: `common` <-
  `symbols` <- `expr` <- `{format, client}` <- `console` <- `debug_adapter`.
- In `ConsoleMain`, `debug::MessageLoopPoll` and its deferred `Cleanup()` must
  outlive `Session`, `Console`, `DebugAdapterServer`, and `BufferedBidiPipe`
  (scoped in an inner block).
- In `RunLocalAgent`, an inner scope destroys `DebugAgent` and
  `BufferedBidiPipe` before calling `message_loop.Cleanup()`.
- The `--core` and `--connect` command-line flags are mutually exclusive.
- When running interactively (without `enable_debug_adapter`) on a TTY
  `STDIN_FILENO`, the process must own the foreground process group
  (`tcgetpgrp(STDIN_FILENO) == getpgrp()`) or exit with `EXIT_FAILURE`.
- When `ForkLocalAgent()` succeeds, the parent process kills and reaps
  `agent_pid` via `waitpid()` on exit (`fit::defer`).

## Side Effects

Running `zxdb` produces these process, I/O, and network side effects:

- Ignores `SIGPIPE` process-wide via `signal(SIGPIPE, SIG_IGN)`.
- Initializes and cleans up global `libcurl` state (`Curl::GlobalInit` /
  `Curl::GlobalCleanup`) and telemetry (`Analytics::Init` /
  `Analytics::CleanUp`).
- Creates non-blocking OS pipes (`pipe2` with `O_NONBLOCK`) and calls `fork()`
  for `--local` on Linux x64, running and reaping a child `DebugAgent`.
- Reads command sequences from `config_files` and `script_file`, and opens
  `stream_files` (`O_RDONLY | O_NONBLOCK`) to stream output to the console.
- Sends `SIGUSR1` via `kill(options.signal_when_ready, SIGUSR1)` after console
  initialization and startup commands complete.

## Verification

Run these commands to build and test `zxdb`:

- Build: `fx build //src/developer/debug/zxdb`
- Test: `fx test zxdb_tests`
