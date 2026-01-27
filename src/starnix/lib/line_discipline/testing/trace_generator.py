# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Generator for Starnix line discipline traces.

This script runs scenarios defined in `scenarios.py` against a PTY pair and records the
interaction to a JSON file. These traces are then used by the `replayer` test runner
to verify the Starnix line discipline implementation.
"""

import argparse
import fcntl
import json
import logging
import os
import select
import struct
import termios
import time
from typing import Any, Dict, List, Optional, Union


def load_scenarios(json_path: str) -> List[Dict[str, Any]]:
    with open(json_path, "r") as f:
        return json.load(f)


# Configure logging
logging.basicConfig(level=logging.INFO, format="%(message)s")
logger = logging.getLogger(__name__)

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


def decompose_flags(value: int, names: List[str]) -> List[str]:
    """Decomposes a flag integer into a list of symbolic names."""
    result = []
    for name in names:
        if hasattr(termios, name):
            flag = getattr(termios, name)
            if (value & flag) == flag:
                result.append(name)
    return result


def get_termios_dict(fd: int) -> Dict[str, Any]:
    """Reads current termios from fd and returns a dict representation."""
    iflag, oflag, cflag, lflag, ispeed, ospeed, cc = termios.tcgetattr(fd)

    return {
        "c_iflag": decompose_flags(iflag, INTERESTING_IFLAGS),
        "c_oflag": decompose_flags(oflag, INTERESTING_OFLAGS),
        "c_lflag": decompose_flags(lflag, INTERESTING_LFLAGS),
        # c_cflag and c_cc can remain as is or be improved later if needed
        # For line discipline, iflag/oflag/lflag are most critical.
    }


def set_termios(fd: int, config: Dict[str, Any]) -> None:
    """Sets termios on fd from a dict representation."""
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
    current_cc = current[6]
    # Make a mutable copy (list)
    cc = list(current_cc)

    if "c_cc" in config:
        for name, value in config["c_cc"].items():
            if hasattr(termios, name):
                idx = getattr(termios, name)
                if idx < len(cc):
                    cc[idx] = value
                else:
                    logger.warning("Index %d out of range for c_cc", idx)
            else:
                logger.warning("Unknown c_cc name %s", name)

    termios.tcsetattr(
        fd,
        termios.TCSANOW,
        [iflag, oflag, cflag, lflag, current[4], current[5], cc],
    )


def encode_data(data: Union[str, bytes, List[int]]) -> Union[str, List[int]]:
    """Encodes data for JSON serialization."""
    # Try to encode as string if valid utf-8, otherwise list of ints
    if isinstance(data, str):
        return data
    try:
        if isinstance(data, list):
            # Already a list (e.g. from previous load?), just return or check content
            return data
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return list(data)


class ScenarioRunner:
    """Runs a single scenario using a PTY pair."""

    def __init__(self, scenario: Dict[str, Any]):
        self.scenario = scenario
        self.master_fd: Optional[int] = None
        self.slave_fd: Optional[int] = None
        self.recorded_events: List[Dict[str, Any]] = []

    def run(self) -> Dict[str, Any]:
        """Runs the scenario and returns the result dict."""
        self.master_fd, self.slave_fd = os.openpty()
        try:
            self._setup_pty()
            self._run_events()
            final_termios = get_termios_dict(self.slave_fd)
        finally:
            if self.master_fd is not None:
                os.close(self.master_fd)
            if self.slave_fd is not None:
                os.close(self.slave_fd)

        return {
            "name": self.scenario["name"],
            "initial_termios": self.scenario.get("initial_termios", {}),
            "events": self.recorded_events,
            "final_termios": final_termios,
        }

    def _setup_pty(self) -> None:
        # Set non-blocking
        for fd in [self.master_fd, self.slave_fd]:
            fl = fcntl.fcntl(fd, fcntl.F_GETFL)
            fcntl.fcntl(fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)

        if "initial_termios" in self.scenario:
            set_termios(self.slave_fd, self.scenario["initial_termios"])

        if "window_size" in self.scenario:
            ws = self.scenario["window_size"]
            winsize = struct.pack(
                "HHHH", ws.get("ws_row", 24), ws.get("ws_col", 80), 0, 0
            )
            fcntl.ioctl(self.slave_fd, termios.TIOCSWINSZ, winsize)

    def _run_events(self) -> None:
        events = self.scenario.get("events", [])
        for evt in events:
            action = evt["action"]
            if action == "write_to_master":
                self._handle_write(self.master_fd, evt, "write_to_master")
            elif action == "write_to_slave":
                self._handle_write(self.slave_fd, evt, "write_to_slave")
            elif action == "read_from_master":
                self._handle_read(self.master_fd, evt, "read_from_master")
            elif action == "read_from_slave":
                self._handle_read(self.slave_fd, evt, "read_from_slave")
            elif action == "sleep":
                time.sleep(evt.get("duration", 0.05))

    def _handle_write(
        self, fd: int, evt: Dict[str, Any], event_type: str
    ) -> None:
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
                os.write(fd, data_bytes)
                written = True
                break
            except BlockingIOError:
                if expect_block:
                    break
                if time.time() - start_time > 1.0:
                    break  # Timeout
                time.sleep(0.01)

        if expect_block:
            if written:
                self.recorded_events.append(
                    {
                        "type": f"{event_type}_unexpected_success",
                        "data": encode_data(data_bytes),
                    }
                )
            else:
                self.recorded_events.append(
                    {
                        "type": f"{event_type}_blocked",
                        "data": encode_data(data_bytes),
                    }
                )
        else:
            if not written:
                # Master write shouldn't usually block
                raise BlockingIOError(f"{event_type} blocked unexpectedly")
            self.recorded_events.append(
                {"type": event_type, "data": encode_data(data_bytes)}
            )

    def _handle_read(
        self, fd: int, evt: Dict[str, Any], event_type: str
    ) -> None:
        # Wait for data to be available
        select.select([fd], [], [], 0.1)

        total_data = b""
        while True:
            try:
                chunk = os.read(fd, 4096)
                if not chunk:
                    break
                total_data += chunk
            except BlockingIOError:
                break

        # If we expected data but got none, check if we should have waited longer?
        # The original code just passed.

        if total_data:
            self.recorded_events.append(
                {
                    "type": event_type,
                    "data": encode_data(total_data),
                }
            )


def main():
    parser = argparse.ArgumentParser(
        description="Generate Starnix line discipline traces."
    )
    parser.add_argument(
        "--out-dir",
        required=True,
        help="Output directory for traces and generated files",
    )

    args = parser.parse_args()

    if not os.path.exists(args.out_dir):
        os.makedirs(args.out_dir)
    if not os.path.exists(os.path.join(args.out_dir, "traces")):
        os.makedirs(os.path.join(args.out_dir, "traces"))

    logger.info("Generating traces to %s...", args.out_dir)

    # Load scenarios
    scenarios_path = os.path.join(os.path.dirname(__file__), "scenarios.json")
    all_scenarios = load_scenarios(scenarios_path)

    # Generate all scenarios
    for scenario in all_scenarios:
        name = scenario["name"]
        logger.info("Generating scenario: %s", name)
        runner = ScenarioRunner(scenario)
        scenario_result = runner.run()
        out_path = os.path.join(args.out_dir, "traces", f"{name}.json")
        with open(out_path, "w") as f:
            json.dump(scenario_result, f, indent=4)
            f.write("\n")

    logger.info("Done generating traces.")

    # Generate traces.gni
    gni_path = os.path.join(os.path.dirname(args.out_dir), "traces.gni")
    logger.info("Generating %s...", gni_path)
    with open(gni_path, "w") as f:
        f.write("# Copyright 2026 The Fuchsia Authors. All rights reserved.\n")
        f.write(
            "# Use of this source code is governed by a BSD-style license that can be\n"
        )
        f.write("# found in the LICENSE file.\n\n")
        f.write("# Generated by trace_generator.py. DO NOT EDIT.\n\n")
        f.write("trace_files = [\n")
        for scenario in all_scenarios:
            name = scenario["name"]
            f.write(f'  "testing/traces/{name}.json",\n')
        f.write("]\n")

    # Generate scenarios.rs
    rs_path = os.path.join(os.path.dirname(args.out_dir), "scenarios.rs")
    logger.info("Generating %s...", rs_path)
    with open(rs_path, "w") as f:
        f.write("// Copyright 2026 The Fuchsia Authors. All rights reserved.\n")
        f.write(
            "// Use of this source code is governed by a BSD-style license that can be\n"
        )
        f.write("// found in the LICENSE file.\n\n")
        f.write("// Generated by trace_generator.py. DO NOT EDIT.\n\n")
        f.write("#[cfg(test)]\n")
        f.write("mod tests {\n")
        f.write("    use test_case::test_case;\n\n")
        for scenario in all_scenarios:
            name = scenario["name"]
            f.write(
                f'    #[test_case("{name}", include_str!("traces/{name}.json"); "{name}")]\n'
            )
        f.write("    fn test_replay_trace(name: &str, json_data: &str) {\n")
        f.write(
            "        crate::testing::tests::test_replay_trace(name, json_data);\n"
        )
        f.write("    }\n")
        f.write("}\n")

    logger.info("Done.")


if __name__ == "__main__":
    main()
