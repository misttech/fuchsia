---
name: fuchsia-debugger
description: Use fx debug cli to debug test failures.
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
> `stackTrace`, `continue`, or `pause`) strictly require **snake_case**
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

2.  **Retrieve Stack Trace:** Get the stack trace for the suspended `thread_id`
    to pinpoint the failure location:
    ```bash
    fx debug cli --json '{"command": "stackTrace", "thread_id": 1}'
    ```
    Example Response:
    ```json
    {
      "success": true,
      "body": {
        "stackFrames": [
          {
            "id": 0,
            "name": "my_test_function",
            "source": {
              "name": "main_test.cc",
              "path": "/src/main_test.cc"
            },
            "line": 42,
            "column": 1
          }
        ],
        "totalFrames": 1
      }
    }
    ```

3.  **Teardown Session:** Once diagnostics are complete, terminate the debugging
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
| **Set/Delete Breakpoint** | `{"command": "break", "file": "<workspace_root_path>", "line": <line_num>, "delete": <optional_bool>}` *(To delete, specify matching file and line)* |
| **Attach Process** | `{"command": "attach", "filter": "<name_or_pid>"}` |
| **Detach Process** | `{"command": "detach", "pid": <pid>}` or `{"command": "detach", "all": true}` |
| **Get Stack Trace** | `{"command": "stackTrace", "thread_id": <thread_id>}` |
| **Continue Thread** | `{"command": "continue", "thread_id": <thread_id>}` |
| **Pause Thread** | `{"command": "pause", "thread_id": <thread_id>}` |
| **Stop Session** | `{"command": "stop"}` |

### Global Parameters
* **Event History Pruning (`ack_seq`)**: Any command payload can include
  `"ack_seq": <seq>` (e.g., `{"command": "get-state", "ack_seq": 5}`). The
  daemon will prune all event history up to the acknowledged sequence to
  optimize memory.

### Performance & Blocking
* **Smart Blocking**: Commands like `pause`, `stackTrace`, and `wait-for-event`
  are blocking operations and may take up to 10 seconds depending on the
  target's execution state.
