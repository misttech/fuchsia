# AI Coding Agent Tooling - Developer & Architecture Guide

## Overview

`tools/agents` provides the backend implementation for `fx agents`, an extensible CLI tool managing AI coding agent configurations, permission profiles, command expansion regexes, and environment lifecycle for Fuchsia developers.

---

## Directory Layout

```text
tools/agents/
├── BUILD.gn                         # GN build definitions with python_library("agents_lib")
├── OWNERS                           # Tool ownership
├── __init__.py                      # Top-level agents package marker
├── main.py                          # CLI dispatcher & subcommand routing entrypoint
├── commands/
│   ├── __init__.py
│   └── setup.py                     # CLI argument parsing and setup orchestration
├── lib/
│   ├── __init__.py
│   ├── config.py                    # Atomic JSON I/O, .tmp swap, and backup creation
│   ├── permissions.py               # Command regex generator, profile manifests loader
│   ├── services.py                  # Multi-repo daemon discovery and systemctl restart
│   └── state.py                     # State journal, rolling backups, and 3-way reconciliation
└── tests/
    ├── __init__.py
    ├── main_test.py                 # CLI dispatcher unit tests
    ├── setup_test.py                # Setup command integration unit tests
    ├── config_test.py               # Config I/O & atomic write unit tests
    ├── permissions_test.py          # Regex expansion & profile unit tests
    ├── services_test.py             # Daemon discovery & restart unit tests
    └── state_test.py                # State journal, rollback & reconciliation unit tests
```

---

## Module Responsibilities

### 1. `main.py` (CLI Dispatcher)
- Top-level `argparse` configuration for `fx agents`.
- Dynamically registers subcommands under `commands/`.
- Routes execution to the selected subcommand handler (`args.func(args)`).

### 2. `commands/setup.py` (Setup Orchestrator)
- Defines arguments for `--profile`, `--status`, `--rollback`, `--reset`, `--state-dir`, `--allow`, `--deny`, `--ask`, `--allow-list`, `--deny-list`, `--ask-list`, `--config`, and `--dry-run`.
- Coordinates grant aggregation across profiles and ad-hoc flags, configuration persistence, and daemon service restarts.

### 3. `lib/config.py` (Atomic JSON Configuration Management)
- **`load_config(path)`**: Safely loads JSON dictionaries, returning `{}` if missing or malformed.
- **`save_config_atomic(path, data)`**: Writes to a hidden temporary file `.{name}.tmp` in the target directory, formats JSON with 2-space indentation and trailing newline, and atomically swaps it using `Path.replace`.
- **`apply_grants(...)`**: Orchestrates three-way reconciliation, backup creation, atomic config persistence, and state journal logging.

### 4. `lib/permissions.py` (Command Expansion & Profile Engine)
- Defines profile specifications (`read-only`, `local-changes`, `external-changes`, `full-access`).
- Generates anchored regex patterns `command(regex:...)` with support for:
  - Environment variable prefixes (`VAR=value ...`).
  - Git global flags (`-C <dir>`, `--no-pager`, etc.) and subcommand flag placement.
  - Sed in-place flags (`-i`, `--in-place`).
  - System binary PATH resolution and `/usr/bin/` <-> `/bin/` aliases.
- Discovers and loads public and vendor-extended permission manifests.

### 5. `lib/services.py` (Multi-Repo Daemon Management)
- Discovers daemon service lists across public and vendor directories (`services.txt`).
- Restarts running user daemons non-blockingly using `systemctl --user try-restart <service>`.

### 6. `lib/state.py` (State Journal & Reconciliation Engine)
- Manages `StateJournal` schema in `~/.local/share/Fuchsia/agents/setup/state.json` (adhering to `$XDG_STATE_HOME` / `$XDG_DATA_HOME`).
- Implements timestamped rolling backups in `~/.local/share/Fuchsia/agents/setup/backups/` capped at `MAX_BACKUPS = 10`.
- Executes three-way grant reconciliation, multi-step rollback (`rollback`), configuration reset (`reset`), and status reporting (`format_status`).

---

## Three-Way Set Reconciliation Mathematics

To prevent configuration drift, lingering rule conflicts, or loss of developer-authored custom rules, `lib/state.py` implements a deterministic 3-way set reconciliation model:

Let $\text{cat} \in \{\text{allow}, \text{deny}, \text{ask}\}$ represent the grant categories.

### 1. User Custom Rule Extraction
Let $E[\text{cat}]$ be the list of existing grants in `config.json`, and $M_{\text{prev}}[\text{cat}]$ be the grants recorded as managed by the previous setup transaction in `state.json`.
The developer's custom rules $U[\text{cat}]$ are computed as:
$$U[\text{cat}] = E[\text{cat}] \setminus M_{\text{prev}}[\text{cat}]$$

### 2. Opposing Category Conflict Resolution
When transitioning across profiles (e.g., from `read-only` where `local_changes.txt` is denied, to `local-changes` where it is allowed), rules moving into $M_{\text{target}}[\text{cat}]$ must not be blocked by lingering entries in opposing categories:
$$\forall \text{cat} \in \{\text{allow}, \text{deny}, \text{ask}\}, \forall r \in M_{\text{target}}[\text{cat}], \forall \text{other} \neq \text{cat}: \quad U[\text{other}] \leftarrow U[\text{other}] \setminus \{r\}$$

### 3. Obsolete Rule Pruning
Rules previously managed that are no longer part of $M_{\text{target}}[\text{cat}]$ (i.e., $r \in M_{\text{prev}}[\text{cat}] \setminus M_{\text{target}}[\text{cat}]$) are automatically retired and excluded from the final grants.

### 4. Final Grant Construction
The final grant list $\text{final}[\text{cat}]$ combines user custom rules and target managed rules, preserving deterministic ordering:
$$\text{final}[\text{cat}] = U[\text{cat}] \cup M_{\text{target}}[\text{cat}]$$

---

## Command Variant Expansion and Regex Generator Mechanics

Grant entries in `config.json` must reliably match how AI agents and developers invoke commands from shells or subagents. `lib/permissions.py` expands human-readable lines into robust grant variants:

### 1. Environment Variable Prefixes
Commands frequently execute with leading environment variables (e.g. `GIT_PAGER=cat git status`). Regexes prepend `ENV_VARS_PREFIX_PATTERN`:
```python
ENV_VARS_PREFIX_PATTERN = rf"([A-Za-z_][A-Za-z0-9_]*={_ARG_VALUE_PATTERN}\s+)*"
```

### 2. Git Global Flags & Force-Push Detection
Git commands allow global flags before the subcommand (e.g. `git -C //src status`) and flags anywhere in the argument list (e.g. `git push origin main --force` vs `git push -f origin HEAD`):
```python
GIT_GLOBAL_FLAGS_PATTERN = (
    rf"(\s+(-C\s+{_ARG_VALUE_PATTERN}"
    rf"|--no-pager|--no-color|--literal-pathspecs|--no-optional-locks|-c\s+{_ARG_VALUE_PATTERN}))*"
)
```

### 3. Fuchsia Tool Global Flags & Chaining (fx, ffx, jiri)
Fuchsia wrapper tools emit direct regexes matching any binary path (`fx`, `scripts/fx`, `/abs/path/fx`) and supported global flags:
- **`fx`**: Handles `-t <target>`, `--dir <out_dir>`, `--enable=...`, `--disable=...`, `-x`, `-xx`, `-i`, `--`.
- **`ffx`**: Handles standalone `ffx` as well as chained `fx [flags] ffx [flags]`, including `--machine json`, `-t <target>`, `-c <config>`, `-v`, and `--isolate-dir`.
- **`jiri`**: Handles `-j <N>`, `-root <dir>`, `-color <mode>`, `-time`, `-v`, `-vv`, and `--show-progress`.

### 4. Sed In-Place Expansion
Sed in-place invocations (`-i`, `-i.bak`, `--in-place`) can execute with combined flags (e.g., `sed -Ei '...'`). The generator creates specialized regexes matching any `-i` flag variant:
```python
pattern = (
    f"command(regex:{ENV_VARS_PREFIX_PATTERN}(\\S+/)?sed\\b"
    f"(?:\\s+{_ARG_VALUE_PATTERN})*\\s+(-[a-zA-Z]*i\\S*|--in-place(\\S*)?)(?:\\s+.*)?)"
)
```

### 5. Binary PATH and System Aliases
For general binaries (e.g. `grep`, `jq`):
- Resolves full absolute path via `shutil.which`.
- Generates mirrored `/usr/bin/` $\leftrightarrow$ `/bin/` aliases if both paths exist.
- For absolute paths passed as inputs, generates basename variants so invocations via `$PATH` are allowed.

---

## Multi-Repo Overlay Discovery Model

Fuchsia uses a multi-repository structure managed by `jiri`. `fx agents` implements transparent overlay discovery across public and vendor trees:

```python
def find_config_dirs(fuchsia_dir: pathlib.Path) -> list[pathlib.Path]:
    candidates = [fuchsia_dir / ".agents" / "config"]
    vendor_dir = fuchsia_dir / "vendor"
    if vendor_dir.is_dir():
        for vendor_child in sorted(vendor_dir.iterdir()):
            if vendor_child.is_dir():
                cfg_dir = vendor_child / ".agents" / "config"
                if cfg_dir.is_dir():
                    candidates.append(cfg_dir)
    return candidates
```
- **Permission Manifests**: `find_permission_dirs` aggregates all `permissions/` directories. Manifests with matching filenames across public and vendor overlays are concatenated.
- **Daemon Services**: `services.txt` files across all overlays are parsed and aggregated in order.

---

## GN Build Packaging & Testing Guidelines

### GN Build Rules (`tools/agents/BUILD.gn`)
The package is structured as a host Python library and individual host unit tests:
```gn
import("//build/python/python_host_test.gni")
import("//build/python/python_library.gni")

_agents_sources = [
  "__init__.py",
  "commands/__init__.py",
  "commands/setup.py",
  "lib/__init__.py",
  "lib/config.py",
  "lib/permissions.py",
  "lib/services.py",
  "lib/state.py",
  "main.py",
]

if (is_host) {
  python_library("agents_lib") {
    library_name = "agents"
    source_root = "."
    sources = _agents_sources
  }

  python_host_test("main_test") {
    main_source = "tests/main_test.py"
    libraries = [ ":agents_lib" ]
  }
  # ...
}
```

### Running Unit Tests
```bash
fx test main_test setup_test config_test permissions_test services_test state_test
```

---

## Adding New Subcommands

To add a new subcommand `fx agents <subcommand>`:

1. **Create Subcommand Module**: Create `tools/agents/commands/<subcommand>.py`.
2. **Implement Handlers**:
   ```python
   import argparse

   def register_subcommand(
       subparsers: argparse._SubParsersAction[argparse.ArgumentParser],
   ) -> argparse.ArgumentParser:
       parser = subparsers.add_parser("<subcommand>", help="...")
       parser.set_defaults(func=run)
       return parser

   def run(args: argparse.Namespace) -> int:
       # Execution logic
       return 0
   ```
3. **Register in `main.py`**:
   ```python
   from agents.commands import <subcommand>, setup

   def create_parser() -> argparse.ArgumentParser:
       # ...
       setup.register_subcommand(subparsers)
       <subcommand>.register_subcommand(subparsers)
       return parser
   ```
4. **Update `BUILD.gn`**: Add `"commands/<subcommand>.py"` to `_agents_sources` in `tools/agents/BUILD.gn`.
5. **Add Tests**: Create `tools/agents/tests/<subcommand>_test.py` and declare `python_host_test("<subcommand>_test")` in `BUILD.gn`.
