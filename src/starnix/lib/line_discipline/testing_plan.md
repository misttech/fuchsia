# Line Discipline Testing Plan

This document outlines the strategy for verifying that the `line_discipline` crate implements the complete semantics of the Linux terminal line discipline.

## Objective

The goal is to ensure `src/starnix/lib/line_discipline` behaves exactly like the Linux line discipline for all valid `termios` configurations, including edge cases, error conditions, and complex interactions between flags.

## Methodology

We will employ a **Data-Driven/Golden-File Testing** approach. Behavior recorded from a real Linux kernel will serve as the ground truth.

### 1. Trace Generation (Linux)

We will create a helper tool (e.g., `term-test-gen`) to run on a Linux host. This tool will:
1.  Open a compatible PTY pair.
2.  Configure the PTY with specific `termios` settings.
3.  Execute a sequence of I/O operations (writing to master, writing to slave).
4.  Record the exact results (data read, signals generated, buffer states).
5.  Serialize the session into a machine-readable "Trace File" (JSON).

### 2. Validation (Starnix)

We will implement unit tests in `line_discipline/lib.rs` that:
1.  Read the Trace Files.
2.  Instantiate a `LineDiscipline` object.
3.  Replay the inputs from the trace.
4.  Assert that the `LineDiscipline` outputs (and internal state queries) match the Linux trace exactly.

## Trace Data Format

The trace files will contain an ordered list of test scenarios. Each scenario consists of an initial configuration and a sequence of events.

```json
{
  "scenarios": [
    {
      "name": "canon_simple_echo",
      "initial_termios": {
        "c_iflag": "...",
        "c_oflag": "...",
        "c_lflag": "...",
        "c_cc": { ... }
      },
      "window_size": { "ws_row": 24, "ws_col": 80 },
      "events": [
        {
          "action": "write_to_master", // Simulates user typing
          "data": "Hello\n"
        },
        {
          "expect": "read_from_master", // Simulates echo
          "data": "Hello\r\n"
        },
        {
          "expect": "read_from_slave", // Simulates application reading
          "data": "Hello\n"
        }
      ]
    }
  ]
}
```

## Coverage Matrix

The testing plan must cover the following areas comprehensively.

### 1. Canonical Mode (`ICANON`)
*   **Line Buffering**: Verify data is only available after a delimiter (NL, EOL, EOL2, EOF).
*   **Line Editing**:
    *   `VERASE` (Backspace): Removing characters.
    *   `VWERASE` (Word Erase): Removing last word.
    *   `VKILL` (Kill Line): Discarding current line.
*   **Buffer Limits**: Verify behavior when the 4096-byte limit is reached (input discarding or processing).
*   **EOF**: `VEOF` character (usually `^D`) pushing buffer without a newline.

### 2. Non-Canonical Mode (`!ICANON`)
*   **Partial Reads**: Behavior when request size < available data.

### 3. Echoing Logic (`ECHO*`)
*   **Basic Echo (`ECHO`)**: Input characters appear on output.
*   **Erasure Echo (`ECHOE`)**:
    *   `BS SP BS` sequence for screen clearing.
    *   Handling of tab erasure (backtracking multiple columns).
*   **Kill Echo (`ECHOK`, `ECHOKE`)**:
    *   `ECHOK`: Newline after kill char.
    *   `ECHOKE`: Erase entire line from display.
*   **Newline Echo (`ECHONL`)**: Echo `\n` even if `ECHO` is off.
*   **Control Echo (`ECHOCTL`)**: Visualizing control chars (e.g., `^C` for 0x03).

### 4. Input Processing (`c_iflag`)
*   **CR/LF Transformations**: `INLCR`, `IGNCR`, `ICRNL`.
*   **Parity/Framing**: `INPCK`, `ISTRIP`, `IGNBRK`, `BRKINT`, `PARMRK` (handling of 0xFF escaping).
*   **Case Conversion**: `IUCLC` (if supported/relevant).

### 5. Output Processing (`c_oflag` + `OPOST`)
*   **CR/LF Transformations**: `ONLCR`, `OCRNL`, `ONLRET`.
*   **Special Character Handling**: `ONOCR` (don't output CR at col 0).
*   **Tab Expansion**: `XTABS` (expand `\t` to spaces based on column).

### 6. Signal Handling (`ISIG`)
*   **Generation**: `VINTR` (`^C` -> SIGINT), `VQUIT` (`^\` -> SIGQUIT), `VSUSP` (`^Z` -> SIGSTOP).
*   **No Flush (`NOFLSH`)**: Verify if queues are flushed or preserved upon signal generation.
*   **Post-Signal State**: Ensure state consistency after signal generation.

### 7. Flow Control (`IX*`)
*   **Software Flow Control**:
    *   `IXON`: `STOP` (`^S`) pauses output, `START` (`^Q`) resumes.
    *   `IXOFF`: Sending `STOP`/`START` to input side to throttle sender.
    *   `IXANY`: Any character restarts output (if stopped).
*   **Restart Behavior**: Ensure output queue drains correctly after restart.

### 8. Edge Cases
*   **UTF-8 Handling (`IUTF8`)**: Correct backspacing over multi-byte characters.

## Implementation Steps

1.  [x] **Scaffold Trace Generator**: Create the Linux host tool to generate `basic_canon.json` and `basic_noncanon.json`.
2.  [x] **Scaffold Test Harness**: Add `tests/replayer.rs` or similar mod in `line_discipline` to parse and execute JSON tests.
3.  [x] **Baseline Validation**: Run the existing `basic` tests and confirm they pass (or fail if implementation is missing `IXON` etc.).
4.  **Incremental Coverage**:
    *   Implement/Fix `IXON`/`IXOFF`.
    *   Implement/Fix `VMIN`/`VTIME`.
    *   Implement/Fix all `ECHO` variants.
    *   Implement/Fix `OPOST` processing (tabs, etc).
5.  **Cross-Check**: Continuously regenerate traces from Linux if ambiguity arises (e.g., "Does `^W` delete punctuation?").

## Future Considerations
*   **Fuzzing**: Generate random `termios` + random input sequences on Linux, record, and replay on Starnix to find unexpected divergences.
