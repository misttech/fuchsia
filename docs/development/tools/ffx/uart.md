# UART Transport {:#uart-transport}

FDomain can run over a physical serial port (UART), allowing developers to
interact with and debug Fuchsia devices that have no active network or USB
connectivity.

## Overview {:#overview}

The UART transport runs the [FDomain][fdomain] protocol over an asynchronous
serial line (such as `/dev/ttyUSB0` on Linux) connected to a Fuchsia target
device's serial console port (exposed under `/dev/class/serial/`).

Communication operates at a high line rate (defaulting to **1,000,000 baud**)
using the **ResendSP** sliding-window framing protocol, which provides
reliable bidirectional delivery with checksums, acknowledgments, and session
tracking across device reboots.

## Prerequisites {:#prerequisites}

### Host Serial Port Permissions {:#host-permissions}

On Linux hosts, accessing TTY character devices (e.g., `/dev/ttyUSB0`) requires
membership in the `dialout` group:

```bash
sudo usermod -a -G dialout $USER
```

You must log out and back in (or run `newgrp dialout`) for the group membership
to take effect.

### Enable UART Target Discovery in `ffx` {:#enable-uart-discovery}

By default, `ffx` target discovery monitors mDNS and USB discovery streams. To
enable the `UartWatcher` subsystem to monitor active UART connections and list
them in `ffx target list`, set `connectivity.enable_uart` to `true`:

```bash
ffx config set connectivity.enable_uart true
```

### Target Hardware Configuration {:#target-configuration}

On physical hardware platforms (such as boards using DesignWare 8250/APB UART
controllers), high-speed serial rates (1,000,000 baud) are calibrated using
Divisor Latch Fraction (DLF) registers to minimize bit timing error.

Because single-port targets share the physical serial line between the kernel
console and userspace services, enabling `ffx` over UART requires handing off
serial port ownership to the userspace FDomain UART driver (`fdomain-uart-driver`)
via kernel command-line arguments (for example, in `local/BUILD.gn` developer
overrides):

* `dev.fdomain.uart=true`: Enables the target-side `fdomain-uart-driver`
  component to discover and bind to the serial port in `/dev/class/serial/`.
* `dev.fdomain.uart.baud=1000000`: Configures the target serial port baud rate.
* `kernel.serial=none`: Disables the Zircon kernel serial console so kernel
  debug prints do not collide with binary `ResendSP` frames.
* `console.shell=false`: Disables the userspace interactive serial console shell
  (`console.cm`).

Note: Disabling `kernel.serial` suppresses raw early-boot serial output and
synchronous kernel panic dumps over the physical UART pins. Once userspace boots
and `ffx` connects, kernel debug logs (`klog` / `dlog`) and component logs remain
available through `ffx log`.

## Connection Management {:#connection-management}

Unlike network or USB interfaces which support automatic background enumeration,
serial ports are shared physical resources with no standard bus discovery
mechanism. The `ffx uart` tool provides explicit subcommands to manage the
connection lifecycle and ensure mutual exclusion across host tools.

### Connecting to a Target {:#connecting}

To establish a UART connection to a physical device:

```bash
ffx -t /dev/ttyUSB0 uart connect
```

*(You may also include the `uart:` prefix, e.g., `ffx -t uart:/dev/ttyUSB0 uart connect`).*

`ffx uart connect` defaults to **1,000,000 baud**. You can specify a custom baud
rate if your board configuration requires it:

```bash
ffx -t /dev/ttyUSB0 uart connect --baud 115200
```

When connecting:
1. The tool acquires an exclusive file lock (`flock`) on the serial device to
   prevent concurrent access from other processes.
2. It configures the TTY in raw mode with 8 data bits, 1 stop bit, no parity
   (8N1), and the specified baud rate.
3. It spawns the `ffx-uart-driver` background daemon to multiplex FDomain
   traffic.
4. Connection metadata is recorded under the `ffx` shared data directory
   (`ffx_uart/`).

### Running Commands over UART {:#running-commands}

Once connected, you can run any standard `ffx` command targeting the UART
device using the `uart:` address prefix:

```bash
ffx -t uart:/dev/ttyUSB0 target echo
```

```bash
ffx -t uart:/dev/ttyUSB0 log
```

### Probing Connection Health {:#probing}

To verify whether a physical serial port is responsive and communicating with
the target's FDomain UART service:

```bash
ffx -t /dev/ttyUSB0 uart probe
```

The `probe` command performs the following checks:
* If a background driver is already active, it verifies driver process health
  and reports the current status.
* If no driver is active, it opens the port temporarily, performs a protocol
  negotiation handshake with the target, measures the round-trip time (RTT), and
  reports whether the service is responsive.

### Listing Active Connections {:#listing}

To view all active UART driver instances and their associated devices:

```bash
ffx uart list
```

Example output:

```none
TARGET                         PID      BAUD     PROTOCOL   STATUS
/dev/ttyUSB0                   12345    1000000  ResendSP Connected
```

### Target Discovery {:#discovery}

When `connectivity.enable_uart` is enabled and the UART background driver is
active, `ffx` automatically discovers the target:

```bash
ffx target list
```

Example output:

```none
NAME                      SERIAL          TYPE           STATE      ADDRS/IP              MANUAL    RCS
fuchsia-54b7-e1d8-7af6    1234567890ab    minimal.x64    Product    [uart:/dev/ttyUSB0]   N         Y
```

`ffx` discovers UART targets passively by monitoring connection metadata
written by `ffx-uart-driver`. The target's nodename and serial number are
obtained by the driver upon connection and stored in metadata, allowing
discovery to detect and enumerate connected devices immediately without
contending for the physical serial port. If the same device is also connected
over USB or network, `ffx` merges the addresses under a single target entry by
matching the hardware serial number.

### Monitoring Status and Metrics {:#status}

To inspect real-time transmission statistics for an active connection:

```bash
ffx -t /dev/ttyUSB0 uart status
```

This queries the driver daemon's local control socket and metadata to report
operational details including:
* Target path, 16-character target ID, daemon PID, and driver log file path.
* Discovered target nodename and hardware serial number.
* Connection status, configured baud rate, and active protocol (`ResendSP`).
* Estimated round-trip latency (`Estimated RTT`), retransmission count, CRC
  checksum errors, connection drops, and handshake failures.
* Last read/write activity timestamps, outgoing queue length, stall detection
  warnings, and the most recent channel error reported by the target (`Last Target Error`).

### Disconnecting {:#disconnecting}

To terminate the background driver and release the serial device lock:

```bash
ffx -t /dev/ttyUSB0 uart disconnect
```

Releasing the connection frees the serial device for other uses, such as
interactive debug consoles or firmware flashing.

### Reconnecting {:#reconnecting}

To restart the background driver using saved metadata (for instance, after a
cable disconnect or transient host issue):

```bash
ffx -t /dev/ttyUSB0 uart connect --reconnect
```

## Architecture and Framing {:#architecture}

```none
+-------------------------------------------------------------+
|                        Host Machine                         |
|                                                             |
|   +------------+        +-------------------------------+   |
|   |  ffx CLI   | <----> | ffx-uart-driver (Daemon)      |   |
|   +------------+ (UNIX  +-------------------------------+   |
|                  Socket)                |                   |
|                                         | (ResendSP Frames) |
|                                         v                   |
|                              [/dev/ttyUSB0] (flock)         |
+-------------------------------------------------------------+
                                          |
                                          | Physical Serial Cable
                                          | (1,000,000 Baud, 8N1)
                                          v
+-------------------------------------------------------------+
|                       Target Hardware                       |
|                                                             |
|                [/dev/class/serial/<instance>]               |
|                                 |                           |
|                                 v                           |
|               +-----------------------------------+         |
|               | fdomain-uart-driver -> RCS        |         |
|               +-----------------------------------+         |
+-------------------------------------------------------------+
```

### ResendSP Framing Protocol {:#framing-protocol}

The UART transport uses the **ResendSP** framing protocol to stream data over
raw byte-oriented serial links:
* **Sliding Window Go-Back-N**: Supports pipelined transmission of up to 32
  in-flight frames to maximize bandwidth utilization across both local serial
  cables and high-latency bridged links.
* **Flow Control & Backpressure**: Cumulative acknowledgments and per-channel
  flow control prevent buffer overflows on embedded targets.
* **Integrity Checksums**: An 8-bit header checksum (`HdrChk`, CRC-8 polynomial
  `0x07`) on the 13-byte frame header and a 32-bit IEEE 802.3 CRC (`CRC-32`) on
  frame payloads detect transmission errors caused by electrical line noise.
* **Reboot Detection**: Session tokens negotiated during the Channel 0 handshake
  allow the host and target to detect resets and resynchronize stream state
  automatically without rebooting the peer.

## Troubleshooting {:#troubleshooting}

* **Permission Denied opening `/dev/ttyUSB0`**: Ensure your user account is in
  the `dialout` group (`id $USER`), and verify device permissions.
* **Device Busy (`EWOULDBLOCK` / `EAGAIN` during `connect`)**: Another process
  holds an exclusive lock on the TTY device. Verify that no other serial
  terminal programs (e.g., `minicom`, `screen`, `picocom`) are open on the port.
* **Handshake Timeout during `connect` or `probe`**: Check that the physical
  TX/RX lines are wired correctly, the bootloader UART mux and clock are
  configured for 1,000,000 baud, and the target booted with
  `dev.fdomain.uart=true`, `kernel.serial=none`, and `console.shell=false`.

<!-- Reference links -->

[fdomain]: /docs/development/tools/ffx/fdomain.md
