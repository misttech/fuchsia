<!--* freshness: { reviewed: '2026-10-12' } *-->

# `ffx_zxdb` Source (`src`)

## Overview

`//src/developer/debug/ffx_zxdb/src` implements the `ffx_zxdb` host Rust
library. It builds `zxdb` arguments, spawns `zxdb`, lists remote `DebugAgent`
instances, and forwards traffic between UNIX and FDomain FIDL sockets.

## Interface

Public modules expose these types and functions:

- `debug_agent::DebuggerProxy`: Enum wrapping either `fdebugger::LauncherProxy`
  or `fdebugger::DebugAgentProxy` (`debug_agent.rs`).
- `debug_agent::DebugAgentSocket`: Manages a temporary local UNIX domain socket
  and forwards connections to a remote `DebugAgent` via `create`,
  `unix_socket_path`, and `forward_one_connection` (`debug_agent.rs`).
- `util::Agent`, `util::get_all_debug_agents`, `util::print_debug_agents`:
  Queries `fdebugger::LauncherProxy` for active `DebugAgent` instances and their
  attached processes, and prints agent tables to `stdout` (`util.rs`).
- `spawn_forward_task`, `forward_to_agent`: Spawns a background task or runs a
  blocking loop that forwards connections from a `DebugAgentSocket` (`lib.rs`).
- `CommandBuilder`: Builder for `std::process::Command` invocations of `zxdb`
  (`new`, `build`, `args`, `connect`, `push_str`, `extend`, `attach`,
  `attach_each`, `attach_koid`, `attach_job_koid`, `break_at`, `execute`,
  `execute_each`) (`command_builder.rs`).
- `Debugger`, `debugger::DebuggerSession`: Controller (`debugger.rs`) that
  resolves the `zxdb` host tool, registers the symbol index, and spawns `zxdb`
  (`launch`, `from_socket`, `path`, `run_with_command`, `start`, `run`,
  `DebuggerSession::wait`).

## Invariants

Callers and implementations must preserve these invariants:

- `DebugAgentSocket::forward_one_connection` accepts the local UNIX socket
  connection before opening the remote FIDL socket so `zxdb` does not hang on
  connect.
- Bidirectional forwarding in `DebugAgentSocket::forward_one_connection` stops
  when either direction (`unix_to_fidl` or `fidl_to_unix`) hits EOF (0 bytes) or
  an I/O error.
- `make_temp_unix_socket` retries up to 10 times to bind
  `debug_agent_<6-char-alphanumeric>.socket` in `std::env::temp_dir()` on
  `io::ErrorKind::AlreadyExists`.
- `Debugger::from_socket` configures `CommandBuilder` with `--unix-connect`
  pointing to the `DebugAgentSocket` path.
- `Debugger::start` passes `--signal-when-ready=<pid>` to `zxdb` and waits for
  `SIGUSR1` before returning the `DebuggerSession`.
- `DebuggerSession::wait` keeps the background socket-forwarding `Task` alive
  for the session lifetime and returns `Ok(())` only if `zxdb` exits with status
  code `0`.
- `util::print_debug_agents` prints 1-based indices for each `Agent` and lists
  at most 3 attached process names per agent before truncating with `...`.

## Side Effects

Running `ffx_zxdb` produces these host and target side effects:

- `DebugAgentSocket::create` creates a temporary UNIX socket file in
  `std::env::temp_dir()`, and `Drop` deletes the file.
- Registers the symbol index via `symbol_index::ensure_symbol_index_registered`
  and resolves the `zxdb` host binary path via `ffx_config::get_host_tool`.
- Spawns the local `zxdb` child process via `std::process::Command::spawn` and
  waits for exit on a blocking thread via `fuchsia_async::unblock`.
- Registers OS signal hooks for `SIGUSR1` (in `Debugger::start`) and
  `SIGTERM`/`SIGINT` over `UnixStream` pipes (in `forward_to_agent`).
- Calls FIDL methods over `fdomain_fuchsia_debugger` (`LauncherProxy`,
  `DebugAgentProxy`, `AgentIteratorProxy`, `AttachedProcessIteratorProxy`) and
  creates FDomain stream sockets.
- Spawns local tasks via `fuchsia_async::Task::local` to copy bytes between the
  local UNIX socket and remote FDomain socket.
- Writes agent listings to `stdout` and writes symbol index or connection errors
  to `stderr`.

## Verification

Run this command to build `ffx_zxdb/src`:

- Build: `fx build //src/developer/debug/ffx_zxdb`
