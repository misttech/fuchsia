<!--* freshness: { reviewed: '2026-10-12' } *-->

# FFX `zxdb` Integration (`ffx_zxdb`)

This crate contains the host Rust library that integrates `zxdb` with `ffx`
subtools (`ffx debug`, `ffx component`, and `ffx test`).

## Interface

The crate root ([src/README.md](./src/README.md)) re-exports these entry points:

- `CommandBuilder`: Builder for `zxdb` command-line arguments.
- `Debugger` and `debugger::DebuggerSession`: High-level controller that spawns
  `zxdb` and manages the background socket-forwarding task.
- `debug_agent::{DebugAgentSocket, DebuggerProxy}`: Local UNIX domain socket
  listener and FDomain FIDL proxy wrapper.
- `spawn_forward_task` and `forward_to_agent`: Asynchronous and blocking
  connection forwarders between UNIX domain sockets and FDomain FIDL sockets.
- `util::{Agent, get_all_debug_agents, print_debug_agents}`: Helpers that query
  and print active target `DebugAgent` instances.

## Invariants

Callers and implementations must preserve these invariants:

- `DebugAgentSocket::forward_one_connection` accepts local UNIX socket
  connections before opening remote FDomain FIDL socket connections.
- `DebuggerSession` keeps background socket-forwarding tasks alive for its
  entire lifetime, and `Debugger::start` blocks until `zxdb` sends `SIGUSR1`.
- Code in this crate follows
  [//src/developer/ffx/AGENTS.md](../../ffx/AGENTS.md) (Rust edition 2024,
  `*_rust_fdomain` bindings, `fho::Result`/`thiserror` in new or refactored
  code, and `#[fuchsia::test]`).

## Side Effects

Using `ffx_zxdb` produces these host and target side effects:

- Creates and deletes temporary UNIX domain socket files in
  `std::env::temp_dir()`.
- Registers the symbol index, spawns the local `zxdb` host binary, and calls
  `fuchsia.debugger` FIDL methods over FDomain.

## Verification

Run this command to build `ffx_zxdb`:

- Build: `fx build //src/developer/debug/ffx_zxdb`
