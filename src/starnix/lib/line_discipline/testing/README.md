# Line Discipline Trace Generator

This directory contains `trace_generator.py`, a utility for generating golden
trace files on Linux to verifying Starnix line discipline behavior.

## Usage

The generator MUST be run on a Linux host (not Fuchsia/Starnix) as it captures
the behavior of the host's PTY implementation.

### Generating Traces

To generate all trace files:

```bash
python3 src/starnix/lib/line_discipline/testing/trace_generator.py --out-dir src/starnix/lib/line_discipline/testing/traces/
```

### Available Suites

The generator includes several suites of scenarios:

*   `canon_basic`: Basic canonical mode (simple echo, backspace, kill line).
*   `ixon_basic`: Software flow control (XON/XOFF).
*   `echo_variants`: ECHO variants like `ECHONL` and `NOFLSH` with signals.
*   `echo_extended`: `ECHOCTL`, `ECHOPRT` (erasing), `ECHOE` details.
*   `input_flags`: Input processing (`IGNCR`, `INLCR`, `ICRNL`).
*   `output_flags`: Output processing (`OCRNL`, `ONOCR`, `ONLRET`, `XTABS`).

## Adding New Tests

1.  Modify `trace_generator.py` to add a new scenario dictionary.
2.  Run the generator to produce the JSON trace.
3.  Add the new JSON trace file to `BUILD.gn` inputs.
4.  Add a new `#[test_case]` entry to `replayer.rs` for the new trace.
