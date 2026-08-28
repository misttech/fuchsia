# AI Coding Agent Setup (`fx agents`)

## Overview

`fx agents` is a Fuchsia developer workflow tool designed to manage configurations, permission grants, and developer tooling prerequisites for AI coding assistants (such as Gemini CLI, Claude, and IDE plugins) operating within the Fuchsia platform source tree.

The tool provides an extensible subcommand architecture, starting with `fx agents setup`, which handles permission profiles, command regex expansion, atomic JSON configuration management, config backups, and daemon service restarts.

---

## What the Tool Does

### 1. Permission Profiles
The `setup` command provides four permission profiles to match developer security and autonomy needs:

- **`read-only`**: Harmless workspace inspection and build/test commands (`git status`, `git diff`, `fx build`, `fx test`, `cat`, `grep`, `rg`, `fd`). Interactive prompts for device and cache operations.
- **`local-changes` (Default)**: Read-only permissions plus local workspace mutations (`git commit`, `git checkout`, `git branch`, `git stash`, `git rebase`, `fx format-code`).
- **`external-changes`**: Local changes plus remote upload and review interactions (`git push`, `jiri upload`, `fx gh pr ...`).
- **`full-access`**: Unrestricted developer operations including device management (`fx ota`, `fx reboot`, `ffx target ...`) and cache destruction (`fx clean`).

### 2. Partitioned Permission Lists
Rules are cleanly maintained in single-purpose text files under `.agents/config/permissions/` across both the public platform tree and vendor extensions:
- **`read_only.txt`**: Safe inspection commands.
- **`local_changes.txt`**: In-tree editing and local VCS operations.
- **`external_changes.txt`**: Upstream pushing, CL uploads, and remote sync.
- **`batch_execution.txt`**: Batch commands and traversal tools (`find`, `xargs`).
- **`device_ops.txt`**: Device reboots, fastboot flashing, and OTA updates.
- **`cache_destruction.txt`**: Build directory cleans and cache wipes.
- **`never_allow.txt`**: Strictly prohibited destructive commands (e.g. `git reset --hard`, `git clean`).

### 3. Regex & Command Variant Expansion
Permissions are expanded into strictly anchored regex patterns (`command(regex:...)`) and shell alias variants:
- **Git**: Matches environment variable prefixes (`GIT_PAGER=cat`), global flags (`git -C <dir>`), and subcommands with force-push protection anywhere in arguments.
- **Fuchsia Tools**: Expands `fx`, `scripts/fx`, `./scripts/fx`, `tools/fx`, and `.jiri_root/bin/fx`.
- **Python**: Expands `python`, `python3`, and `fuchsia-vendored-python` paths.
- **Sed**: Detects `-i` and `--in-place` options and expands regexes matching in-place edits.
- **System Binaries**: Expands `/usr/bin/` $\leftrightarrow$ `/bin/` aliases and resolves binary paths.

## Permission Profiles

`fx agents setup` provides four tiered permission profiles:

| Profile | Purpose | Allowed (`allow`) | Denied (`deny`) | Prompt on Exec (`ask`) |
|---|---|---|---|---|
| **`read-only`** | Safe workspace inspection and build/test verification. | `read_only.txt` | `never_allow.txt`, `local_changes.txt`, `external_changes.txt` | `device_ops.txt`, `cache_destruction.txt`, `batch_execution.txt` |
| **`local-changes`** *(Default)* | Workspace editing, formatting, local commits, emulators, and device management. | `read_only.txt`, `local_changes.txt`, `device_ops.txt` | `never_allow.txt` | `batch_execution.txt`, `external_changes.txt`, `cache_destruction.txt` |
| **`external-changes`** | Local changes plus upstream code reviews, pushes, and remote infra. | `read_only.txt`, `local_changes.txt`, `external_changes.txt`, `device_ops.txt` | `never_allow.txt` | `batch_execution.txt`, `cache_destruction.txt` |
| **`full-access`** | Unrestricted developer operations across all local, external, device, and cache tools. | `read_only.txt`, `local_changes.txt`, `external_changes.txt`, `device_ops.txt`, `cache_destruction.txt`, `batch_execution.txt` | `never_allow.txt` | *(none)* |

### Partitioned Permission Manifests
Rules are maintained in declarative single-purpose text files under `.agents/config/permissions/`:
- **`read_only.txt`**: Safe inspection (`git status`, `git diff`, `fx build`, `fx test`, `cat`, `grep`, `rg`, `fd`).
- **`local_changes.txt`**: In-tree editing and local VCS (`git add`, `git commit`, `git checkout`, `git branch`, `git stash`, `git rebase`, `fx format-code`).
- **`external_changes.txt`**: Upstream publishing and review (`git push`, `jiri upload`, `fx gh pr ...`).
- **`device_ops.txt`**: Target device controls (`ffx target ...`, `fx ota`, `fx reboot`).
- **`cache_destruction.txt`**: Build directory cleans and cache wipes (`fx clean`, `fx clean-build`).
- **`batch_execution.txt`**: Batch traversal and pipeline tools (`find`, `xargs`).
- **`never_allow.txt`**: Strictly prohibited destructive actions (`git reset --hard`, `git clean -fdx`, `rm -rf /`).

---

## Safety Guarantees

1. **`never_allow.txt` Enforcement**: Destructive and unrecoverable operations are strictly placed in `deny` across every profile.
2. **Force-Push Detection Anywhere in Arguments**: The regex expansion engine inspects all git push arguments to detect force-push flags (`--force`, `-f`, `+<ref>`) regardless of argument order or intervening flags.
3. **Atomic Writes**: Safe replacement prevents corruption during interrupted writes.
4. **Pre-Modification Backups**: Creates `.bak` backup before modifying pre-existing configurations.

---

## Usage Examples

### Basic Setup
```bash
# Configure default profile (local-changes)
fx agents setup

# Configure specific profile
fx agents setup --profile read-only
fx agents setup --profile local-changes
fx agents setup --profile external-changes
fx agents setup --profile full-access

# Preview changes without modifying configuration
fx agents setup --profile read-only --dry-run
```

### Custom Grants & Config Path
```bash
# Add specific custom commands
fx agents setup --allow "fx custom-tool" --deny "rm -rf /" --ask "fx ota"

# Include custom rule list files
fx agents setup --allow-list /path/to/extra_allow.txt --deny-list /path/to/extra_deny.txt

# Configure custom config output path
fx agents setup --profile local-changes --config /path/to/config.json
```

---

## Testing Quick Reference

All Python host tests are hermetic and integrated into the Fuchsia build system:
```bash
fx test main_test setup_test config_test permissions_test services_test
```
