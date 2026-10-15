<!--* freshness: { reviewed: '2026-10-12' } *-->

# ZXDB E2E Tests (`e2e_tests`)

This directory contains the test cases and supporting infrastructure for `zxdb`
end-to-end (E2E) tests.

`ScriptTest` registers test cases dynamically from `.script` files in the
`scripts/` directory and runs them via `script_test.cc`. The script runner feeds
commands from each `.script` file to the `zxdb` console and matches the console
output against expected patterns.

## Prerequisites

The test harness reads build-time configuration from environment variables that
`BUILD.gn` exposes as C preprocessor macros:

- `ZXDB_E2E_TESTS_BUILD_TYPE`: Identifies the active build configuration at
  runtime so scripts can skip unsupported build types.
- `ZXDB_E2E_TESTS_SYMBOL_DIR`: **Required** directory containing hard-linked
  DWARF symbols (`symbols_dir_test_data`) for target inferior programs.
- `ZXDB_E2E_TESTS_SCRIPTS_DIR`: **Required** path to the `.script` files under
  `root_out_dir`.
- `ZXDB_E2E_TESTS_FFX_TEST_DATA`: **Required** when `ffx debug` builds as a
  separate subtool. Unused when `ffx debug` compiles directly into `ffx`.

### FFX Isolate

Before registering GoogleTest cases, `FfxDebugAgentBridge`
(`ffx_debug_agent_bridge.cc`) creates an [FFX Isolate][ffx-isolate]. The isolate
connects to the target and exposes a local Unix socket that `zxdb` uses to
connect to `debug_agent` (bridging FIDL and Unix sockets). In normal user
sessions, `ffx debug` creates the target pipe and spawns `zxdb` as a child
process instead.

The FFX Isolate reads these environment variables to connect to the target:

- `FUCHSIA_DEVICE_ADDR`: The IPv4 or IPv6 address of the target. `fx test` and
  CI set this variable automatically. When running the test binary directly,
  read this address from `ffx target list`.
- `FUCHSIA_SSH_PORT`: The target SSH port. `fx test` and CI set this variable
  automatically, or you must provide it when running the binary directly.
- `FUCHSIA_SSH_KEY`: Path to the authorized target SSH private key. Optional for
  local runs when `$HOME/.ssh/fuchsia_ed25519` exists.
- `FUCHSIA_TEST_OUTDIR`: Output directory for `ffx` logs in CI environments.
  Optional for local runs.

`FfxDebugAgentBridge` reads these variables, initializes the FFX isolate in a
temporary directory, and adds the target device. Next, it spawns `ffx debug
connect --agent-only` as a child process while all test cases run. After the
test suite finishes, `FfxDebugAgentBridge` deletes the isolate directory and
stops the isolate daemon.

### Subdirectories

This directory contains these subdirectories:

- `inferiors/`: Target test programs compiled into the `zxdb_e2e_inferiors`
  Fuchsia package.
- `scripts/`: `.script` files replayed by `ScriptTest` (`script_test.cc`) to
  test `zxdb` console workflows.
- `dap_integration_tests/`: Python end-to-end and framework test suites for the
  `zxdb` Debug Adapter Protocol (DAP) server.

## Authoring Rules & Pitfalls

Follow [scripting.md][scripting.md] and the rules below when writing or triaging
`.script` tests.

### Disabling a Single `.script` Test

Prepend `DISABLED_` to a `.script` filename (for example,
`DISABLED_inline_frames.script`) and link a `https://fxbug.dev/<id>` issue.
Disabling one script preserves test coverage across the rest of the suite while
you fix the failure.

### Disabling a Single python DAP Test

Add the decorator `@unittest.skip("...")` with a reason string pointing to a bug link
`https://fxbug.dev/<id>` above a test case.

### Breaking on Functions Instead of Line Numbers

Always set breakpoints on function names rather than source line numbers in
`scripts/*.script` unless the test specifically checks `source-map` settings. A
line-number command such as `until 9` passes locally but fails on CQ builders
when source files are absent:

```zxdb
[zxdb] until 9
> 9
Source file .../e2e_tests/inferiors/async_rust_multithreaded.rs not found.
You might want to adjust the source file remap setting. See "get source-map".
```

The source file provider requires local source files to resolve line-number
display information.

## Interface

Public headers and test directories expose these harnesses:

- `zxdb::ScriptTest`: End-to-end test harness (`script_test.h`,
  `script_test.cc`) that replays `.script` files against a live `zxdb` `Console`
  and `Session`.
- `zxdb::FfxDebugAgentBridge`: Manages an isolated `ffx` environment
  (`FFXIsolate`) and bridges the `debug_agent` Unix socket for E2E tests
  (`ffx_debug_agent_bridge.h`).
- `dap_integration_tests/`: Python test suites (`dap_e2e_tests`,
  `dap_framework_test`) that test the `zxdb` DAP server over an isolated `ffx`
  environment.

## Invariants

Test authors and `BUILD.gn` definitions must preserve these invariants:

- In `scripts/*.script`, break on function names instead of line numbers unless
  testing `source-map`, and wildcard volatile addresses, registers, and KOIDs
  with `??`.
- To disable a failing `.script` test without disabling the suite, rename the
  file with a `DISABLED_` prefix and cite a `https://fxbug.dev/<id>` bug.
- In `BUILD.gn`, place pure host tests (such as `dap_framework_test`) in `tests`
  gated only on `is_host` (`$host_toolchain`) so they run without a board. Gate
  tests on `has_board` only when they require target inferiors or target symbol
  generation (`ZXDB_E2E_TESTS_SYMBOL_DIR` or `DAP_E2E_TESTS_SYMBOL_DIR`).
- When declaring `host_test_data` targets in `BUILD.gn`, list `sources`
  explicitly so CQ sandboxes receive required host binaries such as `zxdb`.
- Keep E2E and integration tests enabled on both `arm64` and `x64`. If a
  platform bug requires an exclusion, document it in `BUILD.gn` with a
  `https://fxbug.dev/<id>` link.
- When testing crash or trap generation (`__builtin_trap()`), do not require
  `ZX_EXCP_SW_BREAKPOINT` (`ud2` on `x86_64` raises
  `ZX_EXCP_UNDEFINED_INSTRUCTION`). Assert that an exception occurred without
  over-constraining the architectural exception code unless testing a specific
  instruction.
- In `dap_integration_tests/`, target Python 3.11+ and never land commented-out
  assertions. Use `await asyncio.wait([prev_task])` to await FIFO predecessor
  tasks without propagating `asyncio.CancelledError`.

## Side Effects

Running the E2E suite produces these host and target side effects:

- Creates and deletes a temporary `ffx` isolate directory, and spawns `ffx debug
  connect --agent-only` as a child process.
- Starts target inferior components from `zxdb_e2e_inferiors` and talks to a
  live Fuchsia device or emulator over SSH.

## Verification

Run these commands to build and run the E2E test suites:

- Build: `fx build //src/developer/debug/e2e_tests:tests`
- Test (all `.script` E2E tests): `fx test --e2e zxdb_e2e_tests`
- Test (single `.script` test): `fx test --e2e zxdb_e2e_tests --
  --gtest_filter="ScriptTest.<script_stem>"`
- Test (DAP suites): `fx test dap_framework_test` and `fx test --e2e
  dap_e2e_tests`

[ffx-isolate]: /docs/development/tools/ffx/development/integration_testing.md
[scripting.md]: /docs/development/debugger/scripting.md
