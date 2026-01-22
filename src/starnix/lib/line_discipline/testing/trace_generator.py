# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import fcntl
import json
import os
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
            os.write(master_fd, data_bytes)
            recorded_events.append(
                {"type": "write_to_master", "data": encode_data(data_bytes)}
            )
            time.sleep(0.01)

        elif action == "write_to_slave":
            data_in = evt["data"]
            data_bytes = (
                data_in.encode("utf-8")
                if isinstance(data_in, str)
                else bytes(data_in)
            )
            os.write(slave_fd, data_bytes)
            recorded_events.append(
                {"type": "write_to_slave", "data": encode_data(data_bytes)}
            )
            time.sleep(0.01)

        elif action == "drain_master" or action == "read_from_master":
            # Read everything currently available
            total_data = b""
            while True:
                try:
                    chunk = os.read(master_fd, 4096)
                    if not chunk:
                        break
                    total_data += chunk
                except BlockingIOError:
                    break
            if total_data:
                recorded_events.append(
                    {
                        "type": "read_from_master",
                        "data": encode_data(total_data),
                    }
                )

        elif action == "drain_slave" or action == "read_from_slave":
            # Read everything currently available
            total_data = b""
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
    ]

    results = []
    for s in scenarios:
        res = run_scenario_single_process(s)
        results.append(res)

    output = {"scenarios": results}
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
