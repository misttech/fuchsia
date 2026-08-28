# AI Coding Agent Tooling (`fx agents`)

## Overview

`fx agents` is a Fuchsia developer workflow tool designed to manage configurations, permission grants, and sidecar state for AI coding assistants (such as Gemini CLI, Claude, and IDE plugins) operating within the Fuchsia platform source tree.

The tool provides an extensible subcommand architecture, centered around `fx agents setup`, which handles:
- **Permission Profile Management**: Declarative profiles matching security and autonomy tiers.
- **Command Variant & Regex Expansion**: Translating commands into strictly anchored permission regexes and tool prefixes.
- **Atomic Configuration Management**: Non-destructive JSON modifications, temporary file swaps, and mandatory backups.
- **State Journaling & Three-Way Reconciliation**: Sidecar tracking in `~/.local/share/Fuchsia/agents/setup/state.json` (respecting `$XDG_STATE_HOME` / `$XDG_DATA_HOME`) preserving user custom rules across profile switches.
- **Lifecycle Management**: Status inspection (`--status`), multi-step rollback (`--rollback [N]`), and configuration reset (`--reset`).
- **Multi-Repo Daemon Orchestration**: Transparent discovery and restarting of developer background daemons.

---

## Quickstart

### Apply Default Permission Profile
Run `fx agents setup` to configure the default `local-changes` profile:
```bash
fx agents setup
```

### Apply a Specific Profile
```bash
fx agents setup --profile read-only
```

### Dry Run Mode
Preview calculated grant modifications and daemon restarts without touching `config.json` or restarting services:
```bash
fx agents setup --profile full-access --dry-run
```

---

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
3. **Non-Destructive Set Reconciliation**: Custom grants previously configured in `config.json` that were not injected by Fuchsia setup are strictly preserved.
4. **Atomic JSON Writes**: Modifications are written to `.config.json.tmp` and swapped via filesystem replacement to prevent file corruption.
5. **Pre-Modification Backups**: Every write creates a timestamped backup in `~/.local/share/Fuchsia/agents/setup/backups/` before modifying existing configuration.

---

## Lifecycle Management Commands

### Status Inspection (`--status`)
Inspect active configuration, rule counts, and history:
```bash
fx agents setup --status
```
Example output:
```text
=== AI Coding Agent Configuration Status ===
Active Profile : local-changes
Last Updated   : 2026-08-26 21:00:00
Fuchsia Root   : /usr/local/google/home/username/fuchsia
Config File    : /home/username/.gemini/config/config.json [exists]
State File     : /home/username/.local/share/Fuchsia/agents/setup/state.json [exists]

Rule Breakdown:
  [ALLOW] :  58 total ( 55 managed,   3 custom)
  [DENY ] :   8 total (  8 managed,   0 custom)
  [ASK  ] :  12 total ( 12 managed,   0 custom)

Profile History (Newest to Oldest):
  1) 2026-08-26 21:00:00 -> [local-changes] (backup: config.20260826_210000.bak)
  2) 2026-08-26 20:30:00 -> [read-only] (backup: config.20260826_203000.bak)
```

### Multi-Step Rollback (`--rollback [N]`)
Revert `config.json` to the state prior to N setup operations:
```bash
# Roll back to previous configuration (N=1)
fx agents setup --rollback

# Roll back 3 setups prior
fx agents setup --rollback 3
```

### Reset Configuration (`--reset`)
Remove all permissions configured by `fx agents setup`, reverting to a clean baseline while preserving any custom developer-authored rules in `config.json`:
```bash
fx agents setup --reset
```
> **Note:** Unlike `--rollback` (which steps backward through previous profile changes using backup snapshots), `--reset` removes all Fuchsia-managed grants and clears the local state journal (`state.json`) without modifying custom permissions or non-permission IDE settings you manually configured.


---

## Ad-Hoc Grants and Custom Lists

Add custom commands or additional grant lists on top of any profile:
```bash
fx agents setup \
  --profile local-changes \
  --allow "fx test //custom:target" \
  --deny "git push upstream main" \
  --allow-list path/to/extra_allowed.txt
```

---

## Multi-Repository Overlays

In multi-repo setups (e.g. `//` with `//vendor/*`), `fx agents` automatically discovers and merges configuration:
1. `.agents/config/permissions/` (Public Fuchsia tree)
2. `vendor/*/.agents/config/permissions/` (Internal & vendor overlays)
3. `services.txt` daemon declarations across all repository roots.
