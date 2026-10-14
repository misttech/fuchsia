---
name: editing-zxdb
description: >
  Workflows for editing, building, testing, and manually checking code under
  //src/developer/debug (zxdb, debug_agent, debug_ipc, shared, ffx_zxdb, and
  e2e_tests).
---

# Editing & Testing `zxdb` and `debug_agent` (`//src/developer/debug`)

Use this skill when you edit, build, test, or manually check features across
`zxdb`, `debug_agent`, `ipc`, `shared`, `ffx_zxdb`, or `e2e_tests`.

## Host Unit Tests (`zxdb`, `ipc`, `shared`)

Run the unified host unit test binary (`zxdb_tests`):

```bash
fx test zxdb_tests
```

To filter specific GoogleTest suites or cases, pass `--gtest_filter` after `--`:

```bash
fx test host_x64/zxdb_tests -- --gtest_filter="DwarfSymbolFactory.*"
```

## Target Backend Tests (`debug_agent`)

Run the test suite that matches the component you edited
(`debug_agent_unit_tests.cm` is the primary target unit test suite):

```bash
# Primary target-side debug_agent package unit tests
fx test debug_agent_unit_tests.cm

# Host-side debug_agent unit tests (for host/Linux-compatible code)
fx test --host debug_agent_unit_tests

# Launcher unit tests (for launcher.cc / debug_agent_launcher)
fx test debug_agent_launcher_unit_tests

# Target-side integration tests
fx test debug_agent_integration_tests
```

### Reloading `/core/debugger` During Manual Device/Emulator Testing

`debug_agent_launcher` runs at `/core/debugger` and manages `debug_agent`
instances in its `#agents` collection. After you edit and build `debug_agent`,
stop `/core/debugger` so the target resolves the updated package on the next
attach:

```bash
ffx component stop /core/debugger
```

## Downstream Verification for DAP, UDS, & `ffx debug`

When you edit `zxdb/debug_adapter`, `ffx_zxdb`, or UDS/DAP protocol bindings,
run the downstream Python CLI, daemon, and `fx test` suites:

1.  **Run Python DAP & CLI test suites**:

    ```bash
    fx test scripts/debug/zxdb_cli src/lib/debug/dap/python/zxdb
    ```

2.  **Trace reverse dependencies via `fx gn refs`**:

    ```bash
    fx gn refs $(fx get-build-dir) \
      "//src/developer/debug/zxdb:zxdb(//build/toolchain:host_x64)"
    ```

    If the output includes `//scripts/fxtest/python:*`, run `fx test
    //scripts/fxtest/python:tests`.

## End-to-End (`.script` & DAP) Tests

Run the E2E suites when you edit target interaction, stepping, symbol lookup, or
DAP handlers (see [e2e_tests/README.md](../../e2e_tests/README.md)):

```bash
# Run a single .script E2E test by filename stem
fx test --e2e zxdb_e2e_tests -- --gtest_filter="ScriptTest.<script_name>"

# Run the full E2E suite and DAP integration tests
fx test --e2e zxdb_e2e_tests
fx test dap_framework_test
fx test --e2e dap_e2e_tests
```

## Manual Feature Verification with `fx debug cli` (`zxdb-cli`)

Use the turn-based `fx debug cli` tool to check `zxdb`, DAP, or `debug_agent`
features on a live target without an interactive TUI. See
[scripts/debug/zxdb_cli/README.md](../../../../scripts/debug/zxdb_cli/README.md)
and [`fuchsia-debugger`](../../skills/fuchsia-debugger/SKILL.md) for full
details:

1.  **Start the background `zxdb` daemon** (starts the package server and
    connects to the target):

    ```bash
    fx debug cli start
    # Or via JSON: fx debug cli --json '{"command": "start"}'
    ```

2.  **Attach, set breakpoints, and inspect state**:

    ```bash
    # Attach to a component or process filter
    fx debug cli --json \
      '{"command": "attach", "filter": "<process_or_component_name>"}'

    # Set a breakpoint and wait for a stop event
    fx debug cli --json \
      '{"command": "break", "file": "src/foo/bar.cc", "line": 42}'
    fx debug cli --json \
      '{"command": "wait-for-event", "last_seen_seq": 0, "timeout": 10}'

    # Inspect session state, threads, stack frames, and variables
    fx debug cli --json '{"command": "get-state"}'
    fx debug cli --json '{"command": "stack-trace", "thread_id": 1}'
    fx debug cli --json \
      '{"command": "variables", "thread_id": 1, "frame_index": 0}'
    ```

3.  **Run `zxdb` console verbs or expressions via `evaluate`** (requires a
    stopped target thread):

    ```bash
    fx debug cli --json \
      '{"command": "evaluate", "thread_id": 1, "frame_index": 0,
        "expression": "sym-info my_var"}'
    fx debug cli --json \
      '{"command": "evaluate", "thread_id": 1, "frame_index": 0,
        "expression": "print my_var"}'
    ```

4.  **Stop the daemon when finished**:

    ```bash
    fx debug cli stop
    ```
