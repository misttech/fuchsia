# ffx uart

This directory contains the `ffx-uart` tool, which is an `ffx` subtool used to
manage and interact with UART (Universal Asynchronous Receiver-Transmitter)
connections on Fuchsia devices.

The tool communicates with physical and virtual Fuchsia target devices via
serial TTY lines or UNIX domain sockets, providing a CLI for managing background
`ffx-uart-driver` daemons and inspecting link health.

## Directory Structure and Files

* **uart/BUILD.gn**: Defines the GN build rules for the `ffx_tool_uart` library,
  the `ffx-uart` tool binary, and host unit tests.
* **src/main.rs**: The executable entry point for the `ffx-uart` CLI tool. It
  initializes the asynchronous runtime and executes `UartTool`.
* **src/lib.rs**: The main entry point for the tool's library code. It declares
  submodules, exposes target resolution and metadata helpers, and dispatches CLI
  subcommands (`connect`, `disconnect`, `list`, `probe`, `status`) to their
  implementations in `src/subtools/`.
* **src/args.rs**: Defines the CLI argument parsing definitions using `argh`,
  specifying subcommands and their command-line parameters.
* **src/subtools/mod.rs**: Declares the subtool implementation modules:
  * **src/subtools/connect.rs**: Spawns the background driver daemon
    (`ffx-uart-driver`), or re-establishes a connection using cached metadata
    when `--reconnect` is passed.
  * **src/subtools/disconnect.rs**: Terminates the background driver and cleans
    up companion socket and metadata files.
  * **src/subtools/list.rs**: Discovers and enumerates active UART connections
    by scanning the metadata directory and verifying daemon liveness.
  * **src/subtools/probe.rs**: Exercises the connection health of a UART target
    via a host-side protocol negotiation handshake (or queries daemon status if
    already connected).
  * **src/subtools/status.rs**: Queries the running daemon's control socket and
    metadata to report real-time communication metrics, stall detection, and
    target channel errors.
* **src/driver.rs**: Implements lifecycle management utilities for the background
  driver daemon, including locating the driver binary, polling for connection
  readiness, validating daemon liveness, and terminating stale processes.
* **src/metadata.rs**: Manages serializing, writing, reading, and updating
  connection state metadata (as JSON files). Associates 16-character hexadecimal
  target identifiers with daemon PIDs, baud rates, protocols, target identity,
  and runtime metrics.
* **src/stream.rs**: Manages connecting to supported transport layers (UNIX
  domain sockets or raw TTY serial ports), configuring TTY parameters (exclusive
  `flock`, custom baud rates, 8N1 raw mode via `termios`), and executing the
  host-side handshake (`run_host_handshake`).
* **src/sys.rs**: Houses system-level utility functions, including peer PID
  lookup on UNIX domain sockets and process liveness checks (filtering out
  zombie processes).
* **src/tests.rs**: The integration and unit test suite for the tool, using
  **test_data/mock_driver.py** to simulate driver daemon behavior and verify
  connection lifecycles, error boundaries, and socket states.
