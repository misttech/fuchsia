# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import fcntl
import json
import os
import select
import struct
import termios
import time

# LFLAGS naming is a bit inconsistent (ICANON, ISIG, ECHO...).
# Let's just list common ones we care about to avoid noise.

INTERESTING_IFLAGS = [
    "IGNBRK",
    "BRKINT",
    "IGNPAR",
    "PARMRK",
    "INPCK",
    "ISTRIP",
    "INLCR",
    "IGNCR",
    "ICRNL",
    "IUCLC",
    "IXON",
    "IXANY",
    "IXOFF",
    "IMAXBEL",
    "IUTF8",
]
INTERESTING_OFLAGS = [
    "OPOST",
    "OLCUC",
    "ONLCR",
    "OCRNL",
    "ONOCR",
    "ONLRET",
    "OFILL",
    "OFDEL",
]
INTERESTING_LFLAGS = [
    "ISIG",
    "ICANON",
    "XCASE",
    "ECHO",
    "ECHOE",
    "ECHOK",
    "ECHONL",
    "ECHOCTL",
    "ECHOPRT",
    "ECHOKE",
    "DEFECHO",
    "FLUSHO",
    "NOFLSH",
    "TOSTOP",
    "PENDIN",
    "IEXTEN",
]


def decompose_flags(value, names):
    result = []
    for name in names:
        if hasattr(termios, name):
            flag = getattr(termios, name)
            if (value & flag) == flag:
                result.append(name)
    return result


def get_termios_dict(fd):
    iflag, oflag, cflag, lflag, ispeed, ospeed, cc = termios.tcgetattr(fd)

    return {
        "c_iflag": decompose_flags(iflag, INTERESTING_IFLAGS),
        "c_oflag": decompose_flags(oflag, INTERESTING_OFLAGS),
        "c_lflag": decompose_flags(lflag, INTERESTING_LFLAGS),
        # c_cflag and c_cc can remain as is or be improved later if needed
        # For line discipline, iflag/oflag/lflag are most critical.
    }


def set_termios(fd, config):
    iflag = 0
    for name in config.get("c_iflag", []):
        if hasattr(termios, name):
            iflag |= getattr(termios, name)

    oflag = 0
    for name in config.get("c_oflag", []):
        if hasattr(termios, name):
            oflag |= getattr(termios, name)

    lflag = 0
    for name in config.get("c_lflag", []):
        if hasattr(termios, name):
            lflag |= getattr(termios, name)

    # Default cflag if not provided
    cflag = termios.CS8 | termios.CREAD | termios.B38400

    # Get current to preserve others
    current = termios.tcgetattr(fd)
    cc = current[6]
    # Make a mutable copy (list)
    cc = list(cc)

    if "c_cc" in config:
        for name, value in config["c_cc"].items():
            if hasattr(termios, name):
                idx = getattr(termios, name)
                if idx < len(cc):
                    cc[idx] = value
                else:
                    print(f"Warning: Index {idx} out of range for c_cc")
            else:
                print(f"Warning: Unknown c_cc name {name}")

    termios.tcsetattr(
        fd,
        termios.TCSANOW,
        [iflag, oflag, cflag, lflag, current[4], current[5], cc],
    )


def encode_data(data):
    # Try to encode as string if valid utf-8, otherwise list of ints
    if isinstance(data, str):
        return data
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return list(data)


def run_scenario_single_process(scenario):
    master_fd, slave_fd = os.openpty()

    # Set non-blocking
    fl = fcntl.fcntl(master_fd, fcntl.F_GETFL)
    fcntl.fcntl(master_fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)
    fl = fcntl.fcntl(slave_fd, fcntl.F_GETFL)
    fcntl.fcntl(slave_fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)

    if "initial_termios" in scenario:
        set_termios(slave_fd, scenario["initial_termios"])

    if "window_size" in scenario:
        ws = scenario["window_size"]
        winsize = struct.pack(
            "HHHH", ws.get("ws_row", 24), ws.get("ws_col", 80), 0, 0
        )
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, winsize)

    events = scenario.get("events", [])
    recorded_events = []

    for evt in events:
        action = evt["action"]

        if action == "write_to_master":
            data_in = evt["data"]
            data_bytes = (
                data_in.encode("utf-8")
                if isinstance(data_in, str)
                else bytes(data_in)
            )
            # Retry loop for write
            start_time = time.time()
            written = False
            while True:
                try:
                    os.write(master_fd, data_bytes)
                    written = True
                    break
                except BlockingIOError:
                    if time.time() - start_time > 1.0:
                        break  # Timeout
                    time.sleep(0.01)

            if not written:
                # If we expected to block, this is fine? No, master write shouldn't block for us usually.
                raise BlockingIOError("write_to_master blocked unexpectedly")

            recorded_events.append(
                {"type": "write_to_master", "data": encode_data(data_bytes)}
            )

        elif action == "write_to_slave":
            data_in = evt["data"]
            data_bytes = (
                data_in.encode("utf-8")
                if isinstance(data_in, str)
                else bytes(data_in)
            )
            expect_block = evt.get("expect_block", False)

            start_time = time.time()
            written = False
            while True:
                try:
                    os.write(slave_fd, data_bytes)
                    written = True
                    break
                except BlockingIOError:
                    if expect_block:
                        break
                    if time.time() - start_time > 1.0:
                        break
                    time.sleep(0.01)

            if expect_block:
                if written:
                    # It succeeded but we expected block.
                    # We should probably record that it wrote, but this might fail the test if strict.
                    # For now, let's treat it as a different event type?
                    recorded_events.append(
                        {
                            "type": "write_to_slave_unexpected_success",
                            "data": encode_data(data_bytes),
                        }
                    )
                else:
                    recorded_events.append(
                        {
                            "type": "write_to_slave_blocked",
                            "data": encode_data(data_bytes),
                        }
                    )
            else:
                if not written:
                    raise BlockingIOError("write_to_slave blocked unexpectedly")
                recorded_events.append(
                    {"type": "write_to_slave", "data": encode_data(data_bytes)}
                )

        elif action == "drain_master" or action == "read_from_master":
            # Read everything currently available
            total_data = b""

            # Wait for data to be available (up to 0.1s to allow propagation)
            # This replaces the unconditional sleeps after writes.
            readable, _, _ = select.select([master_fd], [], [], 0.1)

            # ... existing drain logic ...
            while True:
                try:
                    chunk = os.read(master_fd, 4096)
                    if not chunk:
                        break
                    total_data += chunk
                except BlockingIOError:
                    break

            # If we expected data but got none?
            expected_data = evt.get("data")
            if expected_data and not total_data:
                # If we explicitly asked to read specific data, and got nothing, maybe we should wait a bit?
                # The previous loop handles draining what's *currently* there.
                # Let's add a small poll if we expect data.
                pass

            if total_data:
                recorded_events.append(
                    {
                        "type": "read_from_master",
                        "data": encode_data(total_data),
                    }
                )

        elif action == "drain_slave" or action == "read_from_slave":
            # ... existing drain logic ...
            total_data = b""

            # Wait for data to be available
            select.select([slave_fd], [], [], 0.1)

            while True:
                try:
                    chunk = os.read(slave_fd, 4096)
                    if not chunk:
                        break
                    total_data += chunk
                except BlockingIOError:
                    break
            if total_data:
                recorded_events.append(
                    {"type": "read_from_slave", "data": encode_data(total_data)}
                )

        elif action == "sleep":
            time.sleep(evt.get("duration", 0.05))

    final_termios = get_termios_dict(slave_fd)

    os.close(master_fd)
    os.close(slave_fd)

    return {
        "name": scenario["name"],
        "initial_termios": scenario.get("initial_termios", {}),
        "events": recorded_events,
        "final_termios": final_termios,
    }


def main():
    scenarios = [
        {
            "name": "canon_simple_echo",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "Hello\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "basic_backspace",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "He\x7flo\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "canon_word_erase",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world\x17\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "canon_kill_line",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "ignore me\x15keep\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "canon_echo_ctl",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "A\x01B\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "ixon_basic",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_slave", "data": "Hello\n"},
                {"action": "read_from_master", "data": "Hello\r\n"},
                # Stop output
                {"action": "write_to_master", "data": "\x13"},
                {"action": "sleep", "duration": 0.1},
                # Write more data, should be buffered but not visible.
                # Linux blocks immediately, so we expect blocking.
                {
                    "action": "write_to_slave",
                    "data": "World\n",
                    "expect_block": True,
                },
                # Resuming output
                {"action": "write_to_master", "data": "\x11"},
                # Now write should succeed
                {"action": "write_to_slave", "data": "World\n"},
                # And we should read it
                {"action": "read_from_master", "data": "World\r\n"},
            ],
        },
        {
            "name": "echo_nl",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHONL"
                    # ECHO is explicitly missing
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "a\n"},
                # 'a' is NOT echoed. '\n' IS echoed (as \r\n due to ONLCR).
                {"action": "read_from_master", "data": "\r\n"},
                {"action": "read_from_slave", "data": "a\n"},
            ],
        },
        {
            "name": "noflsh_sigint",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "NOFLSH",  # prevent flushing
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "foo"},
                {"action": "write_to_master", "data": "\x03"},  # ^C (SIGINT)
                {"action": "write_to_master", "data": "\n"},
                # 'foo' should be echoed. ^C echoed as ^C. \n echoed.
                {"action": "read_from_master", "data": "foo^C\r\n"},
                # 'foo' should NOT be discarded.
                {"action": "read_from_slave", "data": "foo\n"},
            ],
        },
        {
            "name": "echo_extended",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOE",
                    "ECHOK",
                    "ECHOCTL",
                    "ECHOKE",
                    "IEXTEN",
                ],
            },
            "events": [
                # ECHOCTL behavior
                {"action": "write_to_master", "data": "Ctl\x01\n"},
                {
                    "action": "read_from_master",
                    "data": "Ctl^A\r\n",
                },  # \x01 echoed as ^A
                {"action": "read_from_slave", "data": "Ctl\x01\n"},
                # ECHOE behavior (already tested in basic_backspace, but double check)
                {"action": "write_to_master", "data": "Erase\x7f\n"},
                {
                    "action": "read_from_master",
                    "data": "Erase\x08 \x08\r\n",
                },  # BS SP BS
                {"action": "read_from_slave", "data": "Eras\n"},
            ],
        },
        {
            "name": "echo_prt",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    # ECHOPRT overrides ECHOE/ECHOKE usually or tries to use it.
                    "ECHOPRT",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "a\x7f\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echo_prt_two_chars",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    # ECHOPRT overrides ECHOE/ECHOKE usually or tries to use it.
                    "ECHOPRT",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "aa\x7f\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world\x17\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase_typing",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abc def\x17ghi\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_kill",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "ignore me\x15keep\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echonl_echoprt",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": ["ISIG", "ICANON", "ECHONL", "ECHOPRT", "IEXTEN"],
            },
            "events": [
                {"action": "write_to_master", "data": "a"},
                {"action": "write_to_master", "data": "\x7f"},
                {"action": "write_to_master", "data": "b\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase_space",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world"},
                # Ctrl+W
                {"action": "write_to_master", "data": "\x17"},
                # Space
                {"action": "write_to_master", "data": " "},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase_tab",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world"},
                # Ctrl+W
                {"action": "write_to_master", "data": "\x17"},
                # Tab
                {"action": "write_to_master", "data": "\t"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase_nl_aln",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world"},
                # Ctrl+W
                {"action": "write_to_master", "data": "\x17"},
                # Newline + 'a'
                {"action": "write_to_master", "data": "\na"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_word_erase_nl_nl",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world"},
                # Ctrl+W
                {"action": "write_to_master", "data": "\x17"},
                # Newline + Newline
                {"action": "write_to_master", "data": "\n\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_kill_nl_aln",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "hello world"},
                # Ctrl+U (KILL)
                {"action": "write_to_master", "data": "\x15"},
                # Newline + 'a'
                {"action": "write_to_master", "data": "\na"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_multi_aln",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Alphanumeric
                {"action": "write_to_master", "data": "xy"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_multi_space",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Space
                {"action": "write_to_master", "data": " "},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_multi_nl",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Newline
                {"action": "write_to_master", "data": "\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_nl_aln",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Newline + Alphanumeric
                {"action": "write_to_master", "data": "\n"},
                {"action": "write_to_master", "data": "a"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_nl_space",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Newline + Space
                {"action": "write_to_master", "data": "\n"},
                {"action": "write_to_master", "data": " "},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "echoprt_backspace_nl_nl",
            "initial_termios": {
                "c_iflag": ["ICRNL", "IXON"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": [
                    "ISIG",
                    "ICANON",
                    "ECHO",
                    "ECHOPRT",
                    "ECHOK",
                    "IEXTEN",
                ],
            },
            "events": [
                {"action": "write_to_master", "data": "abcde"},
                # 3 Backspaces
                {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
                # Newline + Newline
                {"action": "write_to_master", "data": "\n\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
    ]

    # All scenarios map
    all_scenarios = {s["name"]: s for s in scenarios}

    # Add new scenarios for input flags
    input_scenarios = [
        {
            "name": "input_igncr",
            "initial_termios": {
                "c_iflag": ["IGNCR"],
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": ["ICANON", "ECHO"],  # Simplest echo to verify
            },
            "events": [
                {"action": "write_to_master", "data": "a\rb\n"},
                # \r should be ignored. a, b, \n should be echoed.
                # \n echoed as \r\n
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "input_inlcr",
            "initial_termios": {
                "c_iflag": ["INLCR"],
                "c_oflag": ["OPOST"],  # No ONLCR to avoid confusion
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                {"action": "write_to_master", "data": "a\nb\n"},
                {"action": "write_to_master", "data": "\x04"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "input_icrnl_prec",
            "initial_termios": {
                "c_iflag": ["IGNCR", "ICRNL"],  # IGNCR should take precedence
                "c_oflag": ["OPOST", "ONLCR"],
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                {"action": "write_to_master", "data": "a\rb\n"},
                # \r ignored (IGNCR). It does NOT become \n (ICRNL).
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
    ]

    output_scenarios = [
        {
            "name": "output_ocrnl",
            "initial_termios": {
                "c_iflag": ["ICRNL"],
                "c_oflag": ["OPOST", "OCRNL"],
                # OCRNL: Map CR to NL on output.
                # NOTE: ONLCR (NL->CRNL) default is usually on.
                # If ONLCR is ALSO on, then CR -> NL, and NL -> CRNL!
                # Wait, ONLCR affects NL. OCRNL affects CR.
                # If I send CR, and OCRNL is set, it becomes NL.
                # Then if ONLCR is set, does that NL become CRNL?
                # POSIX says "CR characters are converted to NL."
                # It does NOT say this NL is then subject to ONLCR. Usually these are separate transformations.
                # Let's verify Linux behavior.
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                {"action": "write_to_master", "data": "a\r"},  # Input CR.
                # ICRNL: \r -> \n on input.
                # Echo: \n is echoed.
                # Output processing of echoed char:
                # If \n is echoed, ONLCR might apply?
                # Test logic: write to master -> PTY master -> tty line discipline input -> echo -> output processing -> PTY master read.
                # Wait, if I write to master it goes to input queue.
                # Input queue (ICRNL) converts \r to \n.
                # Echo sees \n.
                # Output queue sees \n.
                # ONLCR (if set) converts \n to \r\n.
                # OCRNL affects *output* CR.
                # To test OCRNL, we need the *output* stream to contain a CR.
                # Echoing a \r would do it (if ICRNL is OFF).
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            # Explicit test for OCRNL where we produce a CR in output by avoiding ICRNL
            "name": "output_ocrnl_explicit",
            "initial_termios": {
                "c_iflag": [],  # No ICRNL
                "c_oflag": ["OPOST", "OCRNL"],  # Map Output CR -> NL
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                {"action": "write_to_master", "data": "a\r"},
                # \r input -> \r echoed.
                # Output processing: \r -> \n (OCRNL).
                # Expect 'a', '\n'.
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "output_onocr",
            "initial_termios": {
                "c_iflag": [],
                "c_oflag": ["OPOST", "ONOCR"],  # Don't output CR at column 0
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                # At column 0 (start).
                {"action": "write_to_master", "data": "\r"},
                # Should NOT echo anything (suppressed).
                {"action": "write_to_master", "data": "a\r"},
                # 'a' (col 1), then \r (goes to col 0). Echoed.
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "output_onlret",
            "initial_termios": {
                "c_iflag": [],
                "c_oflag": ["OPOST", "ONLRET"],  # NL performs CR function
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                # ONLRET means NL is assumed to return to col 0.
                # This mostly affects column tracking for tabs?
                # Starnix implementation:
                # if output_flags & ONLRET: column = 0 (on \n)
                # Does it affect the *bytes* sent?
                # "The NL character is assumed to do the carriage-return function; the column pointer will be set to 0. ONLCR is not implemented." (Linux man)
                # Doesn't change bytes, just state.
                # To verify state, we need a TAB.
                # TAB behavior depends on column.
                {"action": "write_to_master", "data": "a\n"},
                {"action": "drain_master"},
                {"action": "drain_slave"},
                {"action": "write_to_master", "data": "\t"},
                # 'a' (col 1). '\n' (col 0 because ONLRET).
                # '\t' should advance to next tab stop (8).
                # If ONLRET was NOT set, '\n' might not reset column?
                # Wait, '\n' normally moves down, not left.
                # So state column would stay at 1?
                # If column stays at 1, '\t' moves to 8.
                # If column resets to 0, '\t' moves to 8.
                # Wait tab stops are fixed (8, 16...).
                # If col is 1, next tab is 8. Added spaces: 7.
                # If col is 0, next tab is 8. Added spaces: 8.
                # SO bytes output will differ!
                # If ONLRET is set, we expect 8 spaces after \n.
                # If ONLRET is NOT set (and no ONLCR outputting \r), column remains?
                # Linux: \n is just LF. Column?
                # Usually LF doesn't change column.
                # So 'a' (1) -> LF -> col 1. Tab -> 7 spaces.
                # With ONLRET: 'a' (1) -> LF -> col 0. Tab -> 8 spaces.
                # WE need XTABS to see spaces, otherwise we just see \t.
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
        {
            "name": "output_xtabs",
            "initial_termios": {
                "c_iflag": [],
                "c_oflag": ["OPOST", "XTABS"],  # Expand tabs to spaces
                "c_lflag": ["ICANON", "ECHO"],
            },
            "events": [
                {"action": "write_to_master", "data": "\t"},
                # 8 spaces.
                {"action": "write_to_master", "data": "a\t"},
                # 'a' + 7 spaces.
                {"action": "drain_master"},
                {"action": "drain_slave"},
            ],
        },
    ]

    for s in input_scenarios:
        all_scenarios[s["name"]] = s
    for s in output_scenarios:
        all_scenarios[s["name"]] = s

    parser = argparse.ArgumentParser(
        description="Generate Starnix line discipline traces."
    )
    parser.add_argument(
        "--out-dir",
        required=True,
        help="Output directory for JSON files",
    )

    args = parser.parse_args()

    if not os.path.exists(args.out_dir):
        os.makedirs(args.out_dir)

    print(f"Generating traces to {args.out_dir}...")

    # Generate all scenarios
    for name, scenario in all_scenarios.items():
        print(f"Generating scenario: {name}")
        scenario_result = run_scenario_single_process(scenario)
        out_path = os.path.join(args.out_dir, f"{name}.json")
        with open(out_path, "w") as f:
            json.dump(scenario_result, f, indent=2)

    print("Done.")


if __name__ == "__main__":
    main()
