# zxsh - Fuchsia Shell

`zxsh` is a modern, lightweight, POSIX-like command shell implemented in Rust.
It is designed specifically for the Fuchsia operating system to serve as a
fast, robust, and minimal shell environment, intended to replace the
NetBSD-derived `dash` shell (`zircon/third_party/uapp/dash`).

## Key Features & Architecture

### Flat, Relocatable AST

Unlike traditional ASTs which rely on recursive heap-allocated structures (such
as `Box<Node>`), `zxsh` compiles commands into a **flat, relocatable Abstract
Syntax Tree** (`src/ast.rs`).

*   **Zero-Copy Subshells**: The AST is serialized into a continuous memory
    buffer using relative 32-bit offset pointers (`RelativePtr32` and
   `RelativeSlice32`) via `zerocopy`.
*   **VMO Transport**: When spawning a subshell, the parent simply writes this
    flat AST buffer and the shell environment directly into a Zircon VMO and
    passes it as a startup handle. The subshell deserializes it in place without
    allocations or parsing overhead.

### Binary Size & Memory Optimizations

Because `zxsh` runs in early-boot and recovery environments, keeping the binary
size small is a primary design goal. It avoids heavy standard library features
and large third-party crates:

*   **Byte-Safe Strings**: Standard Rust `String` and `str` are replaced by
    `BString` and `BStr` types from the standard `bstr` crate. These operate
    directly on arbitrary `[u8]` byte slices (complying with Bourne shell
    path/variable byte semantics) and are compiled without the `"unicode"`
    feature to avoid compiling heavy Unicode/UTF-8 validation tables.
*   **Minimal Error Formatting**: Standard library OS error loading is bypassed
    in favor of a static lookup mapping (`src/errors.rs`) for Zircon and IO
    error codes, saving substantial binary size.
*   **Zero-Allocation Custom Sort**: Uses a custom sorting module
    (`src/sort.rs`) to avoid compiling the standard library's generic sorting
    algorithm.

### Line Editing

In interactive mode, `zxsh` uses a thin Rust FFI wrapper around the minimal C
`linenoise` library (`//zircon/third_party/ulib/linenoise`) for history, prompt
line-editing, and tab completion, keeping external dependencies to a minimum.

---

## Built-in Commands

`zxsh` supports a rich set of builtins, categorized into three types:

### 1. Essential POSIX Built-ins

Core shell control commands implemented directly in `src/builtins/essential.rs`:

*   `cd`, `chdir`, `pwd`
*   `export`, `readonly`, `local`, `unset`
*   `alias`, `unalias`
*   `set`, `shift`, `getopts`
*   `eval`, `exec`, `exit`, `return`
*   `break`, `continue`
*   `wait`
*   `trap` (signal handling)
*   `.` (source), `:`, `true`, `false`

### 2. Convenience Utilities

Common command-line utilities implemented as builtins in
`src/builtins/utils.rs` to allow filesystem manipulation without the overhead
of spawning external processes:

*   `ls` (supports simple and `-l` long-format listing)
*   `cp`, `mv`, `rm` (supports `-r`, `-f`)
*   `mkdir` (supports `-p`)
*   `echo`, `printf`, `test` / `[`
*   `list` (custom utility to display numbered lines of a file)
*   `msleep` (custom utility to sleep for milliseconds)
*   `dump` (custom utility to hexdump files or stdin)

### 3. Fuchsia/Zircon System Control Built-ins

System-level built-ins implemented in `src/builtins/fuchsia.rs`:

*   `dm` / `power`: Connects to `fuchsia.hardware.power.statecontrol.Admin` to
    control device power states:
    *   `dm poweroff` / `dm shutdown` / `power off`
    *   `dm reboot` / `power reboot`
    *   `dm reboot-bootloader` (or `rb`) / `power reboot-bootloader`
    *   `dm reboot-recovery` (or `rr`) / `power reboot-recovery`
*   `k`: Connects to `fuchsia.kernel.DebugBroker` to execute raw kernel debug
    commands directly from the shell (e.g., `k thread`).

---

## Running zxsh

`zxsh` supports three execution modes:

1.  **Interactive REPL**:
    ```bash
    zxsh
    ```
2.  **Command Execution**:
    ```bash
    zxsh -c "echo 'Hello from zxsh!'"
    ```
3.  **Script Execution**:
    ```bash
    zxsh /path/to/script.sh
    ```

---

## Limitations

### File Descriptor Inheritance

Unlike POSIX shells on Linux/macOS, `zxsh` on Fuchsia does not support implicit
inheritance of non-standard file descriptors (FDs other than `0`, `1`, and `2`)
for external commands.

For example, if you run:
```bash
exec 3>file.txt
my_external_command >&3
```
Or:
```bash
my_external_command 3>file.txt
```
The child process `my_external_command` will NOT inherit FD 3. It will start
with FD 3 closed.

This is a design limitation of the Fuchsia process creation model (`fdio_spawn`
/ `fuchsia.process.Launcher`), which requires explicit transfer of handles and
does not support cloning the entire file descriptor table to child processes by
default. This behavior matches the Fuchsia port of `dash`
(`zircon/third_party/uapp/dash`).

Redirections of standard streams (`0`, `1`, `2`) are fully supported for all
commands.

---

## Testing

`zxsh` comes with a comprehensive suite of integration and unit tests,
including parity testing against `dash`.

To run the tests:
```bash
fx test zxsh_tests
```

Unit tests are organized in `src/tests/` and cover language constructs
(pipelines, loops, redirections, expansions) as well as the behavior of
builtins.
