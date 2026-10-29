# Line Discipline

This crate implements the terminal line discipline logic for Starnix.

## Purpose

The line discipline is responsible for the intermediate processing of characters between the terminal device (e.g., a PTY master or a real serial port) and the reading process (e.g., bash). Its responsibilities include:

*   **Canonical Mode Processing**: Buffering input line-by-line, handling backspace (`\x08` or `\x7f`), word erase (`^W`), line kill (`^U`), literal-next (`^V`), reprint (`^R`), and EOF/EOL delimiters (`^D`, `VEOL`, `VEOL2`).
*   **Echoing**: Echoing typed characters back to the output, potentially transforming them (e.g., `ECHOCTL`, `ECHOE`, `ECHOK`, `ECHOKE`, `ECHONL`, `ECHOPRT`).
*   **Signal Generation**: Detecting special control characters (like `^C`, `^\`, `^Z`) and generating corresponding signals (`SIGINT`, `SIGQUIT`, `SIGTSTP`).
*   **Input & Output Processing**: Transforming input (`ISTRIP`, `PARMRK`, `INLCR`, `IGNCR`, `ICRNL`, `IUCLC`, `IXON`, `IXANY`, `IUTF8`) and output (`OPOST`, `OLCUC`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`, `TABDLY`/`XTABS`) characters, as well as `EXTPROC` and `TIOCPKT` packet mode.

## Notes on Flow Control

While the `LineDiscipline` struct maintains `IXON` and `IXOFF` flags, automatic input flow control (throttling) via `IXOFF` (sending `VSTOP`/`VSTART` when the input buffer fills/empties) is **not implemented**. This matches the behavior of Linux PTYs, which only apply throttling logic to hardware serial devices, not pseudo-terminals. PTYs rely on producer-consumer backpressure rather than in-band flow control characters.

## Architecture

This logic was extracted from `starnix_core` to decouple it from kernel data structures and allow standalone testing. The crate is organized into layered modules with strictly downward dependencies (earlier layers never depend on later ones):

1.  **Configuration & Endpoint Leaf Layers (`termios.rs`, `endpoint.rs`)**:
    *   `termios.rs`: Default `termios2` configuration (`get_default_termios`), control-character definitions, PTY `c_cflag` normalization, UTF-8 continuation checks, and `TermiosExt` flag/character queries.
    *   `endpoint.rs`: `TerminalSide` (`Main` / `Replica`), open reference counting (`EndpointRefs`), and `TIOCPKT` packet mode event state (`PacketMode`).
2.  **Unidirectional Buffering (`queue.rs`)**:
    *   `InputBuffer` / `OutputBuffer` traits: Abstractions for data sources and sinks to avoid direct dependencies on Starnix kernel buffer types.
    *   `Queue`: Three-stage unidirectional byte pipeline (`wait_buffers` -> `line_buffer` -> `read_queue`), `VEOF` delimiter preservation, capacity accounting, and mode-switch packet coalescing.
3.  **Output & Echo Discipline (`output.rs`)**:
    *   `OutputDiscipline`: Replica-to-main output processing (`OPOST`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`, `OLCUC`, `XTABS`), cursor column tracking, `IXON` stopped flow-control state, and deferred echo staging/commitment (`ECHO*`, `ECHOPRT`, `ECHOCTL`, tab rubout).
4.  **Input & Canonical Editing Discipline (`input.rs`)**:
    *   `InputDiscipline` and `PendingSignals`: Main-to-replica input flag translations, `EXTPROC`, `IXON`/`IXANY` triggers, `ISIG` signal generation and queue flushing (`!NOFLSH`), and canonical line editing (`VERASE`, `VWERASE`, `VKILL`, `VLNEXT`, `VREPRINT`, `VEOF`, `VEOL`, `VEOL2`).
5.  **Coordinator (`lib.rs`)**:
    *   `LineDiscipline`: Top-level state machine uniting both endpoints, packet mode, `termios2` transitions, `TC*FLUSH` routing, and main/replica read, write, open, close, and poll operations.

## Testing

Each layer defines unit tests in its module (`line_discipline_tests`), and Linux cross-tested trace scenarios are replayed via `testing/mod.rs` (`line_discipline_scenarios_tests`).
