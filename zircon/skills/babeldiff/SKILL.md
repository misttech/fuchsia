---
name: babeldiff
description: >
  Builds and runs the in-tree `babeldiff` tool
  (`zircon/skills/babeldiff/tool`) to check Zircon C++ to Rust migration
  changes (local commits, working tree diffs, or Fuchsia Gerrit CLs) for line-
  by-line correspondence and adherence to `zircon/skills/cpp-to-rust-rubric`.
  Supports text and JSON modes for agents as well as interactive HTML reports
  for humans.
---

# `babeldiff`: C++ to Rust Correspondence Checker

`babeldiff` (located in-tree at
[`zircon/skills/babeldiff/tool`](tool/README.md)) is a static analysis and
side-by-side correspondence diff tool built specifically for the Zircon C++ to
Rust migration. It parses C++ and Rust with `tree-sitter`, pairs each removed or
converted C++ function with its Rust replacement (following FFI shims, type
names, and body similarity), aligns statements and comments side by side, and
flags deviations from
[`zircon/skills/cpp-to-rust-rubric`](../cpp-to-rust-rubric/SKILL.md):

- **Comments** (`comment`): Lost, added, or reworded inline and header doc
  comments (Rubric Section 1.7, Section 3.15).
- **Error Paths** (`error-path`): Differing `zx_status_t` / `Status` codes,
  changed check order, or errors handled on one side and propagated (`?`) on the
  other (Rubric Section 1.1, Section 3.7).
- **Locking** (`lock`): Locks acquired/released in different places, on
  different lock fields, or with different IRQ/preemption policies (`ksync` vs
  C++ `Guard`, Rubric Section 1.5, Section 3.4).
- **Control Flow & Calls** (`control-flow`, `call`, `order`): Branches (`if`,
  `else`, `switch`/`match`), loops, early returns, extra/missing condition
  tests, calls present on only one side, or inline expansion of C++ statement
  macros / repeated helpers (Rubric Section 1.1, Section 1.8).
- **Assertions, Traces, Atomics & Values** (`assert`, `trace`, `atomic`,
  `value`, `pairing`, `unsafe`): Dropped `ASSERT`/`DEBUG_ASSERT`/canary checks,
  dropped `LTRACE` statements, weakened atomic memory orderings, named
  constants/flags/masks present on only one side, diverging duplicate function
  copies, or high `unsafe` block counts in a function (Rubric Section 1.8,
  Section 3.9, Section 4).
- **File-Level Rubric Lints** (`extern-signature`, `unsafe-safety`,
  `shim-logic`, `file-placement`, `provenance-comment`, `invented-lifetime`,
  `unsafe-density`, `mangled-symbol`): Syntax-level checks across changed files
  verifying FFI signature parity, `// SAFETY:` / `# Safety` documentation,
  minimal forwarding-only FFI shims, 1:1 C++ to Rust file placement, absence of
  "Ported from ..." provenance comments, valid reference lifetimes, safe
  facades, and use of `cpp_*`/`rust_*` FFI functions rather than mangled C++
  symbols.

---

## 1. Building `babeldiff`

The `babeldiff` source code lives in-tree at `zircon/skills/babeldiff/tool`.
Never assume `cargo` is in the system `PATH` or that the binary has already been
built.

Every Fuchsia checkout includes a prebuilt Rust toolchain under
`prebuilt/third_party/rust/<host-platform>/bin` (e.g., `linux-x64`,
`linux-arm64`, `mac-x64`, `mac-arm64`). Prepending this directory to `PATH`
provides both `cargo` and `rustc`.

Build the release binary at
`zircon/skills/babeldiff/tool/target/release/babeldiff` (ignored by
`zircon/skills/babeldiff/tool/.gitignore`):

```bash
HOST_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
HOST_ARCH=$(uname -m | sed 's/x86_64/x64/;s/aarch64/arm64/')
PREBUILT_RUST_BIN="$PWD/prebuilt/third_party/rust/${HOST_OS}-${HOST_ARCH}/bin"

PATH="${PREBUILT_RUST_BIN}:$PATH" \
  cargo build --release --manifest-path zircon/skills/babeldiff/tool/Cargo.toml
```

---

## 2. Selecting the Target Change (Local Git vs. Fuchsia Gerrit)

`babeldiff` works best when it can query the git repository (`babeldiff git ...`
or `babeldiff patch -C .`), because `RepoFinder` searches the base commit for:
- Header (`.h`) doc comments attached to C++ declarations,
- Unchanged C++ functions (when a change adds Rust before deleting C++),
- Base-class method comments and class hierarchy overrides,
- Comments that remain in untouched C++ files (preventing false-positive "lost
  comment" issues).

### Case A: Local Git Commit or Commit Range

```bash
# Compare HEAD with HEAD^:
./zircon/skills/babeldiff/tool/target/release/babeldiff git HEAD

# Compare a specific commit or branch range:
./zircon/skills/babeldiff/tool/target/release/babeldiff git <commit-sha>
./zircon/skills/babeldiff/tool/target/release/babeldiff git origin/main..HEAD
```

### Case B: Uncommitted Working Tree Changes

```bash
git diff HEAD | ./zircon/skills/babeldiff/tool/target/release/babeldiff patch -C . --base HEAD
```

### Case C: Fuchsia Gerrit Change (CL)

When the user specifies a Gerrit URL (such as
`https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1796398` or
`https://fuchsia-review.googlesource.com/c/fuchsia/+/1796398/17`) or a numeric
CL ID (`1796398`):

Pass the CL number, patchset, `fxr/` prefix, or review URL directly to
`babeldiff cl` (or `--cl`):

```bash
# Compare the latest patchset of a CL:
./zircon/skills/babeldiff/tool/target/release/babeldiff cl 1796398

# Or with a specific patchset, fxr/ prefix, or URL:
./zircon/skills/babeldiff/tool/target/release/babeldiff cl 1796398/17
./zircon/skills/babeldiff/tool/target/release/babeldiff cl fxr/1796398
./zircon/skills/babeldiff/tool/target/release/babeldiff cl https://fuchsia-review.googlesource.com/c/fuchsia/+/1796398/17

# Using the top-level flag:
./zircon/skills/babeldiff/tool/target/release/babeldiff --cl 1796398
```

`babeldiff` automatically downloads the patch from Gerrit with full file
context, resolves unchanged companion headers and C++ sources from the local
repository, and runs the correspondence check.

> [!IMPORTANT]
> **Exit Status Behavior**: `babeldiff` exits with status **`0`** when no issues
> are found, **`1`** when issues are found (just like `diff`), and **`2`** on
> error. When running `babeldiff` in shell scripts or chaining commands, **do
> not** chain subsequent commands with `&&` (e.g., `babeldiff ... && cp ...`),
> or exit status `1` will short-circuit the pipeline. Use `;` or explicitly
> allow exit code `1`:
> ```bash
> ./zircon/skills/babeldiff/tool/target/release/babeldiff git FETCH_HEAD --format html -o /tmp/report.html || [ $? -eq 1 ]
> ```

---

## 3. Output Modes for Agents vs. Humans

### 3.1. Agent Modes (Text & JSON)

When analyzing a change as an agent, run `babeldiff` in two stages:

1.  **Stage 1: High-Level Summary & Issue Inventory (`--summary
    --issues-only`)**:
   ```bash
   ./zircon/skills/babeldiff/tool/target/release/babeldiff cl ${CL} --summary --issues-only
   ```
   (Or `./zircon/skills/babeldiff/tool/target/release/babeldiff git HEAD
   --summary --issues-only` for local commits.) This prints:
   - Total pairs, issue counts by kind (`control-flow`, `call`, `comment`,
     `error-path`, `lock`, `order`, `assert`, `trace`, `atomic`, `value`,
     `pairing`, `unsafe`), and rubric lint counts,
   - Per-function summary (`errors`, `locks`, `flow`, `comments`) and exact
     `file:line` findings (`!` for issues, `~` for notes),
   - **Unpaired functions** (`< C++` and `> Rust`, with Rust tests and helper
     callers listed separately),
   - **C++ FFI helpers**, **Rust FFI facades**, **FFI shims** (including any
     ambiguous targets), **C++ changed outside the port** (lines that stay C++
     and must be checked by hand), **File placement**, and **Rubric lints**.

2.  **Stage 2: Untruncated Stacked Alignment (`--layout stacked --issues-only -U
    3`)**:
   ```bash
   ./zircon/skills/babeldiff/tool/target/release/babeldiff cl ${CL} --layout stacked --issues-only -U 3
   ```
   `--layout stacked` prints each C++ line directly above the Rust line it
   aligns with without horizontal width truncation, making it ideal for LLM
   context windows. `-U 3` limits output to 3 rows of context around each
   difference.

3.  **Stage 3: Structured JSON (`--format json --issues-only`)** *(optional for
    programmatic processing)*:
   ```bash
   ./zircon/skills/babeldiff/tool/target/release/babeldiff cl ${CL} --format json --issues-only
   ```
   Emits a single JSON object containing `summary`, `pairs` (with `link`,
   `rationale`, `score`, `overrides`, and `findings` array containing `id`,
   `severity`, `category`, `rubric`, `message`, `cpp`, and `rust`),
   `unpaired_cpp`, `unpaired_rust`, `unpaired_rust_tests`,
   `removed_cpp_ffi_helpers`, `rust_facades`, `shims`,
   `cpp_changes_outside_port`, `lints`, and `placement`.

4.  **Resolving Split or Unpaired Functions (`--pair Cpp=Rust`)**: If a large
    C++ function was split into multiple Rust helpers (for example,
    `ClockDispatcher::ClockDispatcher` in C++ split between
    `ClockDispatcherState::init` and `ClockTransformation::init_in` in Rust),
    `babeldiff` pairs the C++ function with its highest-scoring match and lists
    the second Rust function under `Unpaired functions`. Run a targeted second
    pass forcing the alternate pairing to verify that the other part of the C++
    function (and its inline comments) was faithfully ported:
   ```bash
   ./zircon/skills/babeldiff/tool/target/release/babeldiff git FETCH_HEAD \
     --pair ClockDispatcher::ClockDispatcher=ClockTransformation::init_in \
     --layout stacked -U 3
   ```

### 3.2. Human Mode (Interactive HTML Report & Harness Artifact)

`--format html -o <file>.html` generates a single, self-contained HTML file
(with no external CSS/JS/font dependencies) featuring:
- A sidebar of all function pairs color-coded by status (red = issues, amber =
  notes, green = clean) with browser `localStorage` review checkboxes, plus
  sections for rubric lints and file placement,
- Summary cards for **errors**, **locks**, **control flow**, and **comments**,
- Side-by-side aligned source code with cross-language identifier hover
  highlighting (e.g., `subscriber_count_` highlights `subscriber_count`;
  `ZX_ERR_NO_MEMORY` highlights `Status::NO_MEMORY`),
- Keyboard navigation (`j`/`k` next/prev diff, `n`/`p` next/prev function, `x`
  mark reviewed, `f` fold matching rows, `i` issues only, `?` help).

#### Presenting the HTML Report in the Agent Harness (Jetski / Antigravity)

If your harness supports user-facing HTML artifacts
(`<appDataDir>/brain/<conversation-id>/`):

1.  Because `babeldiff` HTML reports are typically 300 KB to 1 MB, **do not**
    pass the full HTML text into `write_to_file`'s `CodeContent` parameter.
2.  First, register the artifact file with `write_to_file` using a short
    placeholder and `ArtifactMetadata: { UserFacing: true, RequestFeedback:
    false, Summary: "..." }`:
   ```
   TargetFile: <artifact_dir>/babeldiff_<cl>.html
   ```
3.  Then overwrite `<artifact_dir>/babeldiff_<cl>.html` directly via
    `run_command`:
   ```bash
   ./zircon/skills/babeldiff/tool/target/release/babeldiff cl ${CL} \
     --format html -o "<artifact_dir>/babeldiff_${CL}.html" || [ $? -eq 1 ]
   ```

---

## 4. Evaluating Findings Against `zircon/skills/cpp-to-rust-rubric`

`babeldiff` is designed for high precision, but agents must still interpret its
findings in the context of the codebase and
[`zircon/skills/cpp-to-rust-rubric/SKILL.md`](../cpp-to-rust-rubric/SKILL.md).

