<!--* freshness: { reviewed: '2026-10-12' } *-->

# Debug (`//src/developer/debug`)

This directory contains the host and target components of the Fuchsia debugger.

## User Documentation

Read the user guide in
[//docs/development/debugger](../../../docs/development/debugger/README.md). For
AI agent rules and subsystem conventions, read [AGENTS.md](./AGENTS.md).

## Subdirectories & System Boundaries

This directory partitions the debugger into these subsystems:

- [debug_agent](./debug_agent/README.md): Target-side debugger stub for Fuchsia
  and experimental Linux. Manages job/process/thread attachment, breakpoints,
  watchpoints, filters, `ProcessLimbo`, stack unwinding, and `debug_ipc`/FIDL
  servers. Low-level POSIX/Linux helpers live in
  [debug_agent/posix](./debug_agent/posix/README.md).
- [ffx_zxdb](./ffx_zxdb/README.md): Host Rust crate that integrates `zxdb` with
  `ffx debug`. Provides command builders, process management, and UNIX domain
  socket to FDomain FIDL socket forwarding
  ([ffx_zxdb/src](./ffx_zxdb/src/README.md)).
- [ipc](./ipc/README.md): Binary wire protocol (`debug_ipc`) shared between
  `zxdb` and `debug_agent`. Defines request/reply and notification records,
  versioned serialization (`MessageReader`, `MessageWriter`), filter matching,
  and exception decoding.
- [shared](./shared/README.md): Cross-platform abstractions shared by `zxdb` and
  `debug_agent`. Provides `MessageLoop`, `StreamBuffer`, buffered I/O streams,
  CPU register metadata, `AddressRange`, `Status`/`Result`, and hierarchical
  logging ([shared/logging](./shared/logging/README.md)).
- [zxdb](./zxdb/README.md): Host debugger frontend layered upward across
  [zxdb/common](./zxdb/common/README.md),
  [zxdb/symbols](./zxdb/symbols/README.md), [zxdb/expr](./zxdb/expr/README.md),
  [zxdb/format](./zxdb/format/README.md),
  [zxdb/client](./zxdb/client/README.md),
  [zxdb/console](./zxdb/console/README.md)
  ([commands](./zxdb/console/commands/README.md)), and
  [zxdb/debug_adapter](./zxdb/debug_adapter/README.md)
  ([handlers](./zxdb/debug_adapter/handlers/README.md)).
- [e2e_tests](./e2e_tests/README.md): Host-driven `.script` end-to-end tests
  (`zxdb_e2e_tests`) and Debug Adapter Protocol (DAP) integration suites
  (`dap_integration_tests`).
- [skills/fuchsia-debugger](./skills/fuchsia-debugger/SKILL.md): Agent skill
  instructions for debugging Fuchsia test failures with `fx debug cli`.
- [.agents/skills/editing_zxdb](./.agents/skills/editing_zxdb/SKILL.md): Agent
  skill workflows for building and testing `zxdb` and `debug_agent`.

## Global Invariants

All components under `//src/developer/debug` enforce these invariants:

- **Host/Target Split & Wire Compatibility**: `zxdb` builds only on the host
  toolchain (`is_host`). It talks to `debug_agent` over the `debug_ipc` binary
  protocol within the `[kMinimumProtocolVersion, kCurrentProtocolVersion]` ABI
  window from the `Hello` handshake.
- **Upward Subsystem Layering**: Dependencies flow strictly upward without
  cycles: `shared` <- `ipc` <- `{debug_agent, zxdb/common <- zxdb/symbols <-
  zxdb/expr <- {zxdb/format, zxdb/client} <- zxdb/console <-
  zxdb/debug_adapter}`.
- **Single-Threaded Event Loop & Weak Lifetimes**: Both `zxdb` and `debug_agent`
  dispatch IPC, socket I/O, and asynchronous callbacks on a thread-bound
  `debug::MessageLoop`. Never hold raw pointers to transient session objects
  (`Target`, `Process`, `Thread`, `Frame`, `Breakpoint`, `DebuggedProcess`,
  `DebuggedThread`, `FormatNode`) across asynchronous callbacks. Use
  `fxl::WeakPtr` for transient objects and `fxl::RefPtr` for reference-counted
  symbol or expression nodes.
- **Dual Bazel/GN Build Definitions**: Do not manually edit `BUILD.gn` files
  below `# BAZEL2GN SENTINEL` markers. Edit the matching `BUILD.bazel` file and
  run `fx bazel2gn`.

## Global Side Effects

Running the debugger produces these target and host side effects:

- **Target OS & Process Mutation**: Attaches to Zircon exception channels,
  suspends and resumes target threads, and patches trap instructions into
  process memory. Also writes CPU general and debug registers, starts components
  and tests via `fuchsia.debugger` and `fuchsia.component`, and claims or
  releases crashed processes in `ProcessLimbo`.
- **Host I/O, Networking, & Subprocesses**: Opens local UNIX domain and TCP
  sockets for `debug_ipc` and DAP connections, and spawns `zxdb` and optional
  local `debug_agent` processes. Also memory-maps local ELF/DWARF binaries,
  downloads debug symbols over HTTP/HTTPS via `libcurl` into local LRU caches,
  and reads or writes terminal streams and `~/.zxdb_history`.

## System Verification

Run these commands to build and test all debugger components:

- Build: `fx build //src/developer/debug //src/developer/debug/zxdb`
- Test: `fx test zxdb_tests debug_agent_unit_tests
  debug_agent_launcher_unit_tests debug_agent_integration_tests`
