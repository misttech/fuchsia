# zxdb_cli

## Problem Understanding

Traditional debugging tools like `zxdb` are designed for interactive, human
workflows. Developers can manage continuous streams of logs and maintain
debugger state across a long session.

However, automated agents and AI assistants operate differently. They work in
discrete turns, struggle with commands that run indefinitely, and can be
overwhelmed by the verbose output of the Debug Adapter Protocol (DAP).

To enable these tools to effectively use `zxdb`, we need a bridge between the
chatty, connection-oriented DAP and a simpler request-response model that fits
their turn-based workflow.

`fx debug cli` solves this by using a background daemon to manage the persistent
debugger connection. The CLI then exposes a clean, summarized interface to run
commands and get immediate feedback without getting stuck in interactive loops
or protocol noise.

## Usage and Behavior

### Fallback to Interactive Session

If the entry-point script is run without the `cli` subcommand (or with arguments
that don't match it), it behaves as a thin wrapper around `ffx debug connect`.
It will pass all arguments directly to the standard interactive `zxdb` session,
maintaining the experience for human developers.

### Automated Package Server

Both the interactive fallback and the background daemon automatically ensure
that the Fuchsia package server is running. This removes the need for developers
(or agents) to manually check and start the package server before beginning a
debugging session.

### Basic Workflow Examples

The CLI allows running a full debugging session using a request-response model.
Below is a standard workflow: starting the daemon, attaching to a process, and
stopping the daemon.

#### Standard CLI Usage

1. **Start the daemon**:
   ```bash
   fx debug cli start
   ```
   Output:
   ```
   Spawning daemon...
   Daemon is ready.
   ```

2. **Attach to a process** (e.g., attaching to `cobalt.cm`):
   ```bash
   fx debug cli attach cobalt.cm
   ```
   Output:
   ```
   {"success": true, "message": "", "body": {"success": true}}
   ```

3. **Query threads**:
   ```bash
   fx debug cli threads
   ```
   Output:
   ```
   {"success": true, "body": {"threads": [{"id": 1, "name": "main"}, {"id": 2, "name": "worker"}]}}
   ```

4. **Pause a thread**:
   ```bash
   fx debug cli pause -t 1
   ```
   Output:
   ```
   {"success": true, "body": {"success": true}}
   ```

5. **Resume a thread**:
   ```bash
   fx debug cli continue 1
   ```
   Output:
   ```
   {"success": true, "body": {"success": true}}
   ```

6. **Get stack trace**:
   ```bash
   fx debug cli stack-trace -t 1
   ```
   Output:
   ```
   {"success": true, "body": {"thread_id": 1, "stack_frames": [{"frame_index": 0, "name": "main", "line": 42}], "total_frames": 1}}
   ```

7. **Discover driver hosts** (`driver-list`, alias `driver-hosts`):
   ```bash
   fx debug cli driver-list --filter xhci
   ```
   Output:
   ```
   {"success": true, "body": {"driver_hosts": [{"host_koid": 12345, "host_name": "driver_host:12345", "drivers": ["fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm"], "devices": ["dev.sys.platform.xhci"], "colocated": false, "attached": false, "transport_hazard": false, "transport_sensitive": []}]}}
   ```

8. **Attach to a driver or node** (`attach` with flags):
   Attach supports mutually exclusive selectors (`<filter>`, `--driver`/`-d`, `--node`/`-n`) and flags (`--wait`, `--weak`, `--recursive`/`-r`, `--job-only`; `--exact` with `<filter>`):
   ```bash
   fx debug cli attach --driver xhci --weak
   fx debug cli attach -n dev.sys.platform.xhci
   fx debug cli attach -d my_driver --wait
   ```
   Output:
   ```
   {"success": true, "message": "", "body": {"driver_host_koid": 12345, "driver_host_name": "driver_host:12345", "matched_driver": "fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm", "colocated_drivers": ["fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm"], "devices": ["dev.sys.platform.xhci"], "attach_mode": "weak", "transport_hazard": false, "hazard_warning": null, "colocated_warning": null, "filter_pattern": null}}
   ```

9. **Set breakpoints** (`break`):
   Set function (`--function`/`-f`, `--condition`/`-c`) or source (`--file`, `--line`, `<file>:<line>`) breakpoints:
   ```bash
   fx debug cli break -f UsbXhci::Init -c "status != 0"
   fx debug cli break --file src/main.cc --line 42
   ```
   Output:
   ```
   {"success": true, "body": {"breakpoints": [{"id": 1, "verified": true, "line": 42}]}}
   ```

10. **Inspect loaded modules** (`modules`):
   ```bash
   fx debug cli modules -p 12345 --filter libfoo
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "modules": [{"name": "libfoo.so", "base": "0x1000", "build_id": "abc123", "symbols_loaded": true, "symbol_file": "/pkg/lib/libfoo.so", "files_indexed": 10, "functions_indexed": 100}]}}
   ```

11. **Inspect address space** (`aspace`):
   ```bash
   fx debug cli aspace -p 12345 --filter code
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "regions": [{"start": "0x1000", "end": "0x2000", "prot": "r--", "size": "0x1000", "vmo_koid": null, "vmo_offset": null, "committed_pages": 1, "name": "code", "depth": 0}], "page_size": 4096, "total_mapped_bytes": 4096, "total_committed_pages": 1, "total_committed_bytes": 4096}}
   ```

12. **Inspect Zircon handles** (`handles`):
   ```bash
   fx debug cli handles -p 12345 --type channel --detail
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "handles": [{"handle_value": "0x10", "type": "channel", "koid": 100, "related_koid": 101, "peer_owner_koid": 200, "rights": ["transfer", "read", "write"], "vmo_name": null, "vmo_size_bytes": null, "cache_policy": null}]}}
   ```

13. **Read memory** (`mem-read`):
   ```bash
   fx debug cli mem-read 0x1000 -s 16 -p 12345
   ```
   Output:
   ```
   {"success": true, "body": {"address": "0x1000", "size": 16, "hex_bytes": "000102030405060708090a0b0c0d0e0f", "u32_le": ["0x03020100"], "u64_le": ["0x0706050403020100"], "rows": [{"address": "0x1000", "bytes_hex": "00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f", "ascii": "................"}]}}
   ```

14. **Stop the daemon**:
   ```bash
   fx debug cli stop
   ```
   Output:
   ```
   {"success": true, "message": "Daemon stopping", "body": null}
   ```

#### JSON CLI Usage (With JSON)

For programmatic environments or agents, the same operations can be invoked via JSON payloads using the `--json` flag.

1. **Start the daemon**:
   ```bash
   fx debug cli --json '{"command": "start"}'
   ```
   Output:
   ```
   Spawning daemon...
   Daemon is ready.
   ```

2. **Attach to a process**:
   ```bash
   fx debug cli --json '{"command": "attach", "filter": "cobalt.cm"}'
   ```
   Output:
   ```
   {"success": true, "message": "", "body": {"success": true}}
   ```

3. **Query threads**:
   ```bash
   fx debug cli --json '{"command": "threads"}'
   ```
   Output:
   ```
   {"success": true, "body": {"threads": [{"id": 1, "name": "main"}, {"id": 2, "name": "worker"}]}}
   ```

4. **Pause a thread**:
   ```bash
   fx debug cli --json '{"command": "pause", "thread_id": 1}'
   ```
   Output:
   ```
   {"success": true, "body": {"success": true}}
   ```

5. **Resume a thread**:
   ```bash
   fx debug cli --json '{"command": "continue", "thread_id": 1}'
   ```
   Output:
   ```
   {"success": true, "body": {"success": true}}
   ```

6. **Get stack trace**:
   ```bash
   fx debug cli --json '{"command": "stack-trace", "thread_id": 1}'
   ```
   Output:
   ```
   {"success": true, "body": {"thread_id": 1, "stack_frames": [{"frame_index": 0, "name": "main", "line": 42}], "total_frames": 1}}
   ```

7. **Discover driver hosts**:
   ```bash
   fx debug cli --json '{"command": "driver-list", "filter": "xhci"}'
   ```
   Output:
   ```
   {"success": true, "body": {"driver_hosts": [{"host_koid": 12345, "host_name": "driver_host:12345", "drivers": ["fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm"], "devices": ["dev.sys.platform.xhci"], "colocated": false, "attached": false, "transport_hazard": false, "transport_sensitive": []}]}}
   ```

8. **Attach to a process or driver**:
   ```bash
   fx debug cli --json '{"command": "attach", "driver": "xhci", "weak": true}'
   ```
   Output:
   ```
   {"success": true, "message": "", "body": {"driver_host_koid": 12345, "driver_host_name": "driver_host:12345", "matched_driver": "fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm", "colocated_drivers": ["fuchsia-pkg://fuchsia.com/xhci#meta/xhci.cm"], "devices": ["dev.sys.platform.xhci"], "attach_mode": "weak", "transport_hazard": false, "hazard_warning": null, "colocated_warning": null, "filter_pattern": null}}
   ```

9. **Set breakpoints**:
   ```bash
   fx debug cli --json '{"command": "break", "function": "UsbXhci::Init", "condition": "status != 0"}'
   ```
   Output:
   ```
   {"success": true, "body": {"breakpoints": [{"id": 1, "verified": true, "line": 42}]}}
   ```

10. **Inspect loaded modules**:
   ```bash
   fx debug cli --json '{"command": "modules", "pid": 12345, "filter": "libfoo"}'
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "modules": [{"name": "libfoo.so", "base": "0x1000", "build_id": "abc123", "symbols_loaded": true, "symbol_file": "/pkg/lib/libfoo.so", "files_indexed": 10, "functions_indexed": 100}]}}
   ```

11. **Inspect address space**:
   ```bash
   fx debug cli --json '{"command": "aspace", "pid": 12345, "filter": "code"}'
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "regions": [{"start": "0x1000", "end": "0x2000", "prot": "r--", "size": "0x1000", "vmo_koid": null, "vmo_offset": null, "committed_pages": 1, "name": "code", "depth": 0}], "page_size": 4096, "total_mapped_bytes": 4096, "total_committed_pages": 1, "total_committed_bytes": 4096}}
   ```

12. **Inspect Zircon handles**:
   ```bash
   fx debug cli --json '{"command": "handles", "pid": 12345, "type": "channel", "detail": true}'
   ```
   Output:
   ```
   {"success": true, "body": {"pid": 12345, "handles": [{"handle_value": "0x10", "type": "channel", "koid": 100, "related_koid": 101, "peer_owner_koid": 200, "rights": ["transfer", "read", "write"], "vmo_name": null, "vmo_size_bytes": null, "cache_policy": null}]}}
   ```

13. **Read memory**:
   ```bash
   fx debug cli --json '{"command": "mem-read", "address": "0x1000", "size": 16, "pid": 12345}'
   ```
   Output:
   ```
   {"success": true, "body": {"address": "0x1000", "size": 16, "hex_bytes": "000102030405060708090a0b0c0d0e0f", "u32_le": ["0x03020100"], "u64_le": ["0x0706050403020100"], "rows": [{"address": "0x1000", "bytes_hex": "00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f", "ascii": "................"}]}}
   ```

14. **Stop the daemon**:
   ```bash
   fx debug cli --json '{"command": "stop"}'
   ```
   Output:
   ```
   {"success": true, "message": "Daemon stopping", "body": null}
   ```


### Command Summary

| Command | CLI Syntax | JSON Command |
|---|---|---|
| `start` | `start [--port <p>]` | `start` |
| `stop` | `stop` | `stop` |
| `driver-list` | `driver-list [<filter>]` or `driver-list -f <filter>` | `driver-list` |
| `attach` | `attach (<filter> \| --driver <d> \| --node <n>)` | `attach` |
| `break` | `break (<file>:<line> \| -f <sym>) [-c <expr>]` | `break` |
| `threads` | `threads` | `threads` |
| `pause` | `pause [-t <id>] [-p <pid>]` | `pause` |
| `continue` | `continue <thread_id>` | `continue` |
| `stack-trace` | `stack-trace (-t <id> \| -p <pid>) [-r] [--force-remote-unwind] [--unwinder <name>]` | `stack-trace` |
| `modules` | `modules [-p <pid>] [-f <filter>] [-r]` | `modules` |
| `aspace` | `aspace [addr] [-p <pid>] [-f <filter>]` | `aspace` |
| `handles` | `handles [-p <pid>] [--type <t>] [-k <koid>] [--detail]` | `handles` |
| `mem-read` | `mem-read <addr> [-s <size>] [-p <pid>] [-t <id>]` | `mem-read` |


## Technical Architecture

The system is built around two primary components: a long-lived background
**Daemon** and a short-lived **CLI** wrapper (`fx debug cli`).

*   **The Daemon**: This process maintains the continuous connection to the
    debugger. It starts the package server and `zxdb`, connects to the DAP
    server, and listens on a Unix Domain Socket (UDS) for requests. It queues
    critical events and maintains the aggregate state of the session.
*   **The CLI (`fx debug cli`)**: This is a stateless tool designed for
    automated agents. It connects to the daemon via UDS, sends a command,
    receives the response, and exits.

### How They Fit Together

```mermaid
graph TD
    subgraph Host Machine
        AI["Automated Agent"] -->|Invokes| CLI["fx debug cli"]
        CLI -->|"UDS (JSON Protocol)"| Daemon["Daemon Binary"]
        Daemon -->|"DAP (TCP Port 15678)"| Zxdb["zxdb"]
        Daemon -->|Manages| PkgServer["Package Server"]
    end
    subgraph Fuchsia Target
        Zxdb -->|"Debug IPC"| DebugAgent["Debug Agent"]
        DebugAgent -->|"syscalls"| TargetApp["Target Application"]
    end
```

### Communication Protocol

The protocol between the CLI and the Daemon uses a simplified, non-strict
version of the Debug Adapter Protocol (DAP), transmitted as newline-delimited
JSON over the Unix Domain Socket.

#### Smart Proxy and Schemas

To maximize efficiency for automated agents, the daemon acts as a smart proxy.
It strips away verbose protocol metadata and boils messages down to their
essential fields. This ensures that response payloads are compact and
token-efficient.

Example `get-state` Response:
```json
{
  "processes": [
    {
      "id": "p1",
      "name": "target_app",
      "threads": [
        {
          "id": 1,
          "name": "main"
        }
      ]
    }
  ]
}
```

## Ecosystem Fit

This tool layers on top of Fuchsia's existing debug infrastructure. It does not
replace `zxdb`; instead, it leverages `zxdb`'s DAP support (found in
`src/lib/debug/dap/python`) to provide a machine-friendly interface. By bridging
the gap between interactive streams and request-response patterns, it enables
automated tools to use our standard debugging stack effectively.
