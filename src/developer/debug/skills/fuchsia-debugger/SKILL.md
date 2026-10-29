---
name: fuchsia-debugger
description: Use fx debug cli to debug test failures, processes, and live device drivers.
---

# Overview

`fx debug cli` is a machine-readable, agent-friendly interface to `zxdb`. It
uses the Debug Adapter Protocol (DAP) to enable turn-based interactions that
agents are already accustomed to and possess knowledge of. Unlike traditional
console-based debuggers, `fx debug cli` exposes a stateless command-line
frontend that issues request-response style commands to the backend, allowing
agents to send and receive structured JSON as input and output.

`fx debug cli` integrates with `fx test` to allow automatic pausing during test
failures, which allows Agents to inspect the state and diagnose the failure.

---

## Testing Workflow

This mode delegates the management of the debugging session to the testing
framework. The framework automatically starts the `zxdb-daemon` process and
connects to the active target.

### Step 1: Start the Test in the Background
Run `fx test` with the `--agent-debugging-mode` flag. Execute this command
asynchronously in the background so you can proceed with other work while
observing the output:

```bash
fx test <test_suite_name> --agent-debugging-mode
```

*Note: This automatically starts the `zxdb-daemon` background process, which
begins listening for messages sent via `fx debug cli`.*

### Step 2: Poll for Target Transitions
Begin polling the active daemon process for events. Start with `last_seen_seq:
0` and increment the sequence number as events are received:

```bash
fx debug cli --json '{"command": "wait-for-event", "last_seen_seq": 0, "timeout": 10}'
```

> [!NOTE]
> **Socket Startup Delay**: The background daemon may take 1-2 seconds to
> initialize and bind the listening socket after `fx test` begins. If the
> initial command fails with `Daemon socket not found`, wait briefly and retry.

#### Example `wait-for-event` Response (Test Failure / Breakpoint):

```json
{
  "success": true,
  "events": [
    {
      "seq": 1,
      "event": "stopped",
      "body": {
        "threadId": 1,
        "reason": "breakpoint"
      }
    }
  ]
}
```

> [!WARNING]
> **Thread ID Key Casing Mapping Pitfall**:
> Output events returned by `wait-for-event` use **camelCase** keys (e.g.
> `"threadId": 1`). However, subsequent request commands (such as
> `stack-trace`, `continue`, or `pause`) strictly require **snake_case**
> parameters (e.g. `"thread_id": 1`).
> Always map `"threadId"` to `"thread_id"` when programmatically invoking
> requests.

Simultaneously, monitor the output and status of the background `fx test` task.

### Step 3: Handle Execution Scenarios

Depending on the status of the test run, handle one of the following scenarios:

#### Scenario A: Test Failure (Debugger Suspended)
If `wait-for-event` returns a `"stopped"` event, it indicates a test failure or
breakpoint hit. Perform diagnostics:

1.  **Query Session State:** Retrieve all threads and processes active in the
    session to identify the failing thread:

    ```bash
    fx debug cli --json '{"command": "get-state"}'
    ```

    Example Response:

    ```json
    {
      "success": true,
      "body": {
        "threads": [
          { "id": 1, "name": "initial-thread" }
        ],
        "processes": { "12345": "my_test_binary" }
      }
    }
    ```

    *Note: In the `get-state` thread list representation, the key is `"id"`. Map
    this value to the `"thread_id"` parameter in subsequent requests.*

2.  **Retrieve Stack Trace:** Get the stack trace for a suspended thread
    (`thread_id`) or across all threads in a process (`pid`):

    ```bash
    # Thread-level stack trace
    fx debug cli --json '{"command": "stack-trace", "thread_id": 1}'

    # Process-level stack trace (all threads in "pid")
    fx debug cli --json '{"command": "stack-trace", "pid": 12345}'
    ```

    Example Thread Response:

    ```json
    {
      "success": true,
      "body": {
        "thread_id": 1,
        "stack_frames": [
          {
            "frame_index": 0,
            "name": "my_test_function",
            "source": {
              "name": "main_test.cc",
              "path": "/src/main_test.cc"
            },
            "line": 42,
            "column": 1
          }
        ],
        "total_frames": 1
      }
    }
    ```

    Example Process Response:

    ```json
    {
      "success": true,
      "body": {
        "process_id": 12345,
        "stacks": [
          {
            "thread_id": 1,
            "stack_frames": [
              {
                "frame_index": 0,
                "name": "my_test_function",
                "source": {
                  "name": "main_test.cc",
                  "path": "/src/main_test.cc"
                },
                "line": 42,
                "column": 1
              }
            ],
            "total_frames": 1
          },
          {
            "thread_id": 2,
            "stack_frames": [
              {
                "frame_index": 0,
                "name": "worker_thread_entry",
                "source": {
                  "name": "worker.cc",
                  "path": "/src/worker.cc"
                },
                "line": 105,
                "column": 5
              }
            ],
            "total_frames": 1
          }
        ]
      }
    }
    ```

3.  **Inspect Local Variables:** Retrieve variables and arguments within a
    specific stack frame:

    ```bash
    fx debug cli --json '{"command": "variables", "thread_id": 1, "frame_index": 0}'
    ```

4.  **Evaluate Dynamic Expressions:** Evaluate expressions directly within a
    stack frame via `evaluate`.

    Note that `evaluate` requires the target thread to be stopped prior to
    invocation. Ensure the thread is suspended via a breakpoint or `pause`
    command before evaluating.

    Pass the expression directly (do not prefix with `print`):

    ```bash
    fx debug cli --json '{"command": "evaluate", "thread_id": 1, "frame_index": 0, "expression": "my_var"}'
    ```

    Example Response:

    ```json
    {
      "success": true,
      "body": {
        "result": "42",
        "type": "int"
      }
    }
    ```

    To execute raw `zxdb` console commands that do not have a dedicated CLI
    subcommand (such as symbol inspection or disassembly), use the `console`
    command (optionally scoped to a `thread_id` and `frame_index`):

    ```bash
    fx debug cli --json '{"command": "console", "thread_id": 1, "frame_index": 0, "input": "sym-info my_var"}'
    ```

5.  **Teardown Session:** Once diagnostics are complete, terminate the debugging
    session. This detaches from targets and lets the background test runner exit
    cleanly:

    ```bash
    fx debug cli --json '{"command": "stop"}'
    ```

#### Scenario B: Clean Pass (All Tests Succeed)
If all tests pass cleanly, the background `fx test` task will complete
successfully, exiting with status code `0` and summarizing `FAILED: 0` in its
output.

* **Edge Case: Poll Connection Failures**: Since `fx test` automatically reaps
  the background daemon process on a clean exit, any active, blocking
  `wait-for-event` command will terminate abruptly with a connection error
  (e.g., connection refused or closed socket).
* **Action**: If `wait-for-event` returns a connection failure, check the status
  of the background `fx test` task. If it completed successfully (exit code 0),
  treat this as a successful clean pass, print a success message, and exit. **No
  manual `"stop"` command or teardown is required.**

#### Scenario C: Proactive Breakpoint Installation (Non-Failing Paths)
By default, the testing framework attaches to targets weakly (`attach --weak`).
In this state, symbols are not loaded upfront, and dynamic `break` requests will
remain pending and unresolved until a crash occurs.

To debug a passing path or install breakpoints before test execution begins:

1.  Start the test with the `--breakpoint` option to force a normal attach with
    immediate symbol loading (paths must be fully qualified from the workspace
    root):

    ```bash
    fx test <test_name> --agent-debugging-mode --breakpoint <source_file>:<line>
    ```

2.  Poll for the breakpoint hit stopped event via `wait-for-event`.
3.  Once suspended, perform diagnostics and resume execution using `continue`.

---

## JSON Request Reference

All commands are sent as serialized JSON payloads to `fx debug cli --json
'<payload>'`.

| Action | Command Input Payload |
|---|---|
| **Wait for Event** | `{"command": "wait-for-event", "last_seen_seq": <seq>, "timeout": <secs>}` |
| **Get State** | `{"command": "get-state"}` *(Returns active threads, processes, and breakpoints)* |
| **List Threads** | `{"command": "threads"}` |
| **Set/Delete Breakpoint** | `{"command": "break", "file": "<workspace_root_path>", "line": <line_num>, "condition": "<optional_expr>", "delete": <optional_bool>}` or `{"command": "break", "function": "<symbol>", "condition": "<optional_expr>", "delete": <optional_bool>}` *(CLI: `break <file>:<line>` or `break --file <file> --line <line>` or `break -f <symbol> [-c <expr>]`)* |
| **Attach Process / Driver** | `{"command": "attach", "filter": "<name_or_pid>", "weak": <optional_bool>, "exact": <optional_bool>, "recursive": <optional_bool>, "job_only": <optional_bool>}` or `{"command": "attach", "driver": "<driver_name_or_url>", "weak": <optional_bool>, "wait": <optional_bool>}` or `{"command": "attach", "node": "<moniker>", "weak": <optional_bool>, "wait": <optional_bool>}` *(CLI: `attach (<filter> | --driver <drv> | --node <moniker>)`)* |
| **List Driver Hosts** | `{"command": "driver-list", "filter": "<optional_substr>"}` *(CLI: `driver-list [<filter>]` or `driver-list -f <filter>`)* |
| **List Modules** | `{"command": "modules", "pid": <optional_pid>, "filter": "<optional_substr>", "force_reload": <optional_bool>}` *(CLI: `modules [-p <pid>] [-f <filter>] [-r]`)* |
| **Inspect Address Space** | `{"command": "aspace", "pid": <optional_pid>, "address": "<optional_addr>", "filter": "<optional_substr>"}` *(CLI: `aspace [addr] [-p <pid>] [-f <filter>]`)* |
| **Inspect Handles** | `{"command": "handles", "pid": <optional_pid>, "type": "<optional_type>", "koid": <optional_koid>, "detail": <optional_bool>}` *(CLI: `handles [-p <pid>] [--type <type>] [-k <koid>] [--detail]`)* |
| **Read Memory** | `{"command": "mem-read", "address": "<addr_or_expr>", "size": <bytes>, "pid": <optional_pid>, "thread_id": <optional_thread_id>, "frame_index": <frame_index>}` *(CLI: `mem-read <addr_or_expr> [-s <size>] [-p <pid>] [-t <id> [--frame-index <idx>]]`)* |
| **Async Backtrace** | `{"command": "async-backtrace", "pid": <optional_pid>}` *(CLI: `async-backtrace [-p <pid>]`)* |
| **Detach Process** | `{"command": "detach", "pid": <pid>}` or `{"command": "detach", "all": true}` *(CLI: `detach [<pid>] [--all]`)* |
| **Get Stack Trace** | `{"command": "stack-trace", "thread_id": <thread_id>}` or `{"command": "stack-trace", "pid": <pid>, "raw": <optional_bool>}`; optional `"force_remote_unwind": <bool>`, `"unwinder": "cfi" \| "fp" \| "scs" \| "plt" \| "armehabi" \| "sigreturn"` *(CLI: `stack-trace (-t <id> \| -p <pid>) [-r] [--force-remote-unwind] [--unwinder <name>]`)* |
| **List Variables** | `{"command": "variables", "thread_id": <thread_id>, "frame_index": <frame_index>}` *(CLI: `variables -t <id> --frame-index <idx>`)* |
| **Evaluate Expression** | `{"command": "evaluate", "thread_id": <thread_id>, "frame_index": <frame_index>, "expression": "<expr>"}` *(CLI: `evaluate -t <id> [--frame-index <idx>] <expr>`)* |
| **Run Console Command** | `{"command": "console", "input": "<cmd>", "thread_id": <optional_thread_id>, "frame_index": <frame_index>}` *(CLI: `console [-t <id>] [--frame-index <idx>] <cmd>`)* |
| **Step In** | `{"command": "step-in", "thread_id": <thread_id>}` *(CLI: `step-in <id>`)* |
| **Step Over (Next)** | `{"command": "next", "thread_id": <thread_id>}` *(CLI: `next <id>`)* |
| **Step Out (Finish)** | `{"command": "finish", "thread_id": <thread_id>}` *(CLI: `finish <id>`)* |
| **Continue Thread** | `{"command": "continue", "thread_id": <thread_id>}` *(CLI: `continue <id>`)* |
| **Pause Thread / Process** | `{"command": "pause", "thread_id": <thread_id>}` or `{"command": "pause", "pid": <pid>}` *(CLI: `pause -t <id>` or `pause -p <pid>`)* |
| **Stop Session** | `{"command": "stop"}` |

### Global Parameters

* **Event History Pruning (`ack_seq`)**: Any command payload can include
  `"ack_seq": <seq>` (e.g., `{"command": "get-state", "ack_seq": 5}`). The
  daemon will prune all event history up to the acknowledged sequence to
  optimize memory.

### Performance & Blocking

* **Smart Blocking**: Commands like `pause`, `stack-trace`, and `wait-for-event`
  are blocking operations and may take up to 10 seconds depending on the
  target's execution state. Unlike those commands, `evaluate` is non-blocking
  and fails immediately if the target thread is not already stopped.

---

## Live Process & Driver Debugging (`fx debug cli`)

`fx debug cli` also provides workflows for live process inspection and DFv2
driver bring-up on running targets.

### 1. Discover & Safely Attach to a Driver Host

Because pausing a `driver_host` freezes all colocated drivers in that process
(which can sever USB/CDC-NCM/ADB transport if transport or watchdog drivers are
colocated), inspect driver host topology with `driver-list` before attaching:

```bash
# List running driver_hosts, colocated drivers, and transport hazards
fx debug cli driver-list --filter xhci

# Attach by driver name/URL or device node moniker (warns on transport hazards)
fx debug cli attach --driver xhci
fx debug cli attach --node dev.sys.platform.xhci
```

#### Driver Host Transport Hazards & Guidance

When a `driver_host` colocates transport or watchdog drivers (such as USB,
CDC-NCM, ethernet, serial, or watchdog), avoid stopping breakpoints or manual
pause commands to prevent severing host `ffx` communication. Instead:

* Prefer weak attach (`--weak`) to catch crashes without stopping threads.
* Use non-stopping inspection commands (`modules`, `aspace`, `handles`,
  `mem-read`) to examine process state safely.

```bash
# Attach weakly for crash-only catch on transport-sensitive driver hosts
fx debug cli attach --driver xhci --weak
```

#### Pending or Unbound Drivers (`--wait`)

When attaching to a driver that has not yet started or bound, pass `--wait`. If
no running `driver_host` matches, `fx debug cli` installs a zxdb process filter
on `"driver_host"` that attaches to every newly started driver host (since zxdb
matches process names rather than driver URLs or device monikers).

Attach flags (`--weak`, `--recursive`, `--job-only`) are forwarded to this
filter, and the response includes `requested_target` and `filter_warning`
alerting the user that all new driver hosts will be attached.

```bash
# Pre-arm a filter before a driver binds or starts
fx debug cli attach --driver my_driver --wait
```

### 2. Inspect a Live Process

These commands return structured JSON and do not stop the target. When more than
one process is attached, pass `--pid` (`-p`) to choose one.

```bash
# Inspect loaded modules/DSOs, virtual memory mappings, and Zircon handles
fx debug cli modules --filter xhci
fx debug cli aspace --filter mmio
fx debug cli handles --type interrupt --detail

# Read memory at an address (or at a frame-relative expression with -t)
fx debug cli mem-read 0x7ffee1234000 -s 32
```

`modules` requests the module list from `debug_agent`, which ends a weak attach.

### 3. Breakpoints and Stack Traces

```bash
# Set function or conditional breakpoints
fx debug cli break --function usb_xhci::UsbXhci::Init --condition "status != 0"

# If an unwind stops early, retry on the target or force a specific unwinder
fx debug cli stack-trace -p <pid> --force-remote-unwind
fx debug cli stack-trace -t <tid> --unwinder fp
```
