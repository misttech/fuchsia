# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import fcntl
import os
import termios
import time


def test_ixon_buffering():
    master_fd, slave_fd = os.openpty()

    # Set non-blocking on slave
    fl = fcntl.fcntl(slave_fd, fcntl.F_GETFL)
    fcntl.fcntl(slave_fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)

    # Configure IXON
    attrs = termios.tcgetattr(slave_fd)
    iflag, oflag, cflag, lflag, ispeed, ospeed, cc = attrs

    # Enable IXON, disable other processing for clarity
    iflag |= termios.IXON
    # Ensure start/stop chars are default (^Q/^S)
    cc[termios.VSTART] = b"\x11"  # ^Q
    cc[termios.VSTOP] = b"\x13"  # ^S

    termios.tcsetattr(
        slave_fd,
        termios.TCSANOW,
        [iflag, oflag, cflag, lflag, ispeed, ospeed, cc],
    )

    print("Writing 'Hello'")
    os.write(slave_fd, b"Hello\n")

    print("Reading from master")
    out = os.read(master_fd, 1024)
    print(f"Read: {out}")

    print("Sending STOP (^S) to master")
    os.write(master_fd, b"\x13")
    time.sleep(0.1)

    print("Writing 'World' to slave (expecting success/buffering)")
    try:
        n = os.write(slave_fd, b"World\n")
        print(f"Wrote {n} bytes")
    except BlockingIOError:
        print("Write BLOCKED immediately!")
    except OSError as e:
        print(f"Write failed: {e}")

    print("Sending START (^Q) to master")
    os.write(master_fd, b"\x11")
    time.sleep(0.1)

    print("Reading from master")
    try:
        out = os.read(master_fd, 1024)
        print(f"Read: {out}")
    except BlockingIOError:
        print("Read nothing")

    os.close(master_fd)
    os.close(slave_fd)


if __name__ == "__main__":
    test_ixon_buffering()
