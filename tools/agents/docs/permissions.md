# AI Coding Agent Tooling: Permissions & Configuration Architecture

## 1. Overview & Architecture

`tools/agents` manages configurations, permission profiles, command expansion regexes, and
`PreToolUse` command hooks for AI coding agents operating in the Fuchsia source tree.

To let agents run commands productively without clobbering your workspace, the subsystem
implements:

- **Permission Profiles**: Declarative profiles grouped by workflow needs.
- **Permission Manifest Files**: Text files defining allowed, denied, and prompt-on-exec commands.
- **`PreToolUse` Command Hook Engine**: Schema-driven `PreToolUse` hook (`lib/agent_hooks`) installed
  in `~/.gemini/config/hooks.json` that decomposes compound shell commands and enforces `deny` rules and
  in-place `sed -i` gating across flag permutations, nested subcommands, and Turbo mode.
- **Three-Way Set Reconciliation**: Preserves custom rules in `config.json` when switching profiles.
- **Command Variant & Regex Expansion**: Expands `allow` and `ask` commands into regexes that handle
  environment variables, global flags, and tool wrappers, while emitting canonical literal
  `command(<rule>)` grants for `deny` rules.
- **Multi-Repo Overlay Discovery**: Discovers and merges permission files and daemon configs across
  `//` and `//vendor/*`.
- **State Tracking & Backups**: Tracks managed rules in
  `~/.local/share/Fuchsia/agents/setup/state.json` with multi-step rollback and atomic writes.

---

## 2. Permission Profiles & Manifest Files

### 2.1 Tiered Profiles

`fx agents setup` provides four tiered permission profiles:

| Profile                         | Purpose                                                                                | Allowed (`allow`)                                                                                                              | Denied (`deny`)   | Prompt on Exec (`ask`)                                                                   |
| ------------------------------- | -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ----------------- | ---------------------------------------------------------------------------------------- |
| **`read-only`**                 | Safe workspace inspection and build/test verification.                                 | `read_only.txt`                                                                                                                | `never_allow.txt` | `external_changes.txt`, `device_ops.txt`, `cache_destruction.txt`, `batch_execution.txt` |
| **`local-changes`** *(Default)* | Workspace editing, formatting, local commits, emulators, and device management.        | `read_only.txt`, `local_changes.txt`, `device_ops.txt`                                                                         | `never_allow.txt` | `batch_execution.txt`, `external_changes.txt`, `cache_destruction.txt`                   |
| **`external-changes`**          | Local changes plus upstream code reviews, pushes, and remote infra.                    | `read_only.txt`, `local_changes.txt`, `external_changes.txt`, `device_ops.txt`                                                 | `never_allow.txt` | `batch_execution.txt`, `cache_destruction.txt`                                           |
| **`full-access`**               | Unrestricted developer operations across all local, external, device, and cache tools. | `read_only.txt`, `local_changes.txt`, `external_changes.txt`, `device_ops.txt`, `cache_destruction.txt`, `batch_execution.txt` | `never_allow.txt` | *(none)*                                                                                 |

### 2.2 Permission Manifest Files

Rules are defined in declarative text files under `.agents/config/permissions/`:

- **`read_only.txt`**: Safe inspection (`git status`, `git diff`, `fx build`, `fx test`, `cat`,
  `grep`, `rg`, `fd`).
- **`local_changes.txt`**: In-tree editing and local VCS (`git add`, `git commit`, `git checkout`,
  `git branch`, `git stash`, `git rebase`, `fx format-code`, `sed -i`).
- **`external_changes.txt`**: Upstream publishing and review (`git push`, `jiri upload`,
  `fx gh pr ...`).
- **`device_ops.txt`**: Target device controls (`ffx target ...`, `fx ota`, `fx reboot`).
- **`cache_destruction.txt`**: Build directory cleans and cache wipes (`fx clean`,
  `fx clean-build`).
- **`batch_execution.txt`**: Batch traversal and pipeline tools (`find`, `xargs`).
- **`never_allow.txt`**: Destructive actions that are always blocked across all profiles and modes
  (`git reset --hard`, `git clean -f`, `git push --force`, `git push --force-with-lease`, `rm -rf /`, `rm -rf ~`, `mkfs`).

### 2.3 Multi-Repository Overlay Discovery

Fuchsia uses a multi-repository structure managed by `jiri`. `fx agents` discovers overlays across
public and vendor trees via [`lib/paths.py`](../lib/paths.py):

- **Permission Manifests**: `find_permission_dirs` aggregates all `permissions/` directories across
  `//.agents/config/` and `//vendor/*/.agents/config/`. Manifests with matching filenames across
  public and vendor overlays are concatenated.
- **Daemon Services**: `services.txt` files across all overlays are parsed and aggregated in order.

---

## 3. Three-Way Set Reconciliation

To prevent configuration drift, lingering rule conflicts, or loss of developer-authored custom
rules, [`lib/state.py`](../lib/state.py) implements a 3-way set reconciliation model.

Let $\text{cat} \in \{\text{allow}, \text{deny}, \text{ask}\}$ represent the grant categories.

### 3.1 Finding Custom Rules

Let $E[\text{cat}]$ be the list of existing grants in `config.json`, and
$M_{\text{prev}}[\text{cat}]$ be the grants recorded as managed by the previous setup run in
`state.json`. The developer's custom rules $U[\text{cat}]$ are computed as:
$$U[\text{cat}] = E[\text{cat}] \setminus M_{\text{prev}}[\text{cat}]$$

### 3.2 Resolving Category Conflicts

When transitioning across profiles (e.g., from `local-changes` where `external_changes.txt` is
prompted, to `external-changes` where it is allowed), rules moving into $M_{\text{target}}[\text{cat}]$ must not be
blocked by lingering entries in opposing categories:
$$\forall \text{cat} \in \{\text{allow}, \text{deny}, \text{ask}\}, \forall r \in M_{\text{target}}[\text{cat}], \forall \text{other} \neq \text{cat}: \quad U[\text{other}] \leftarrow U[\text{other}] \setminus \{r\}$$

### 3.3 Removing Old Managed Rules

Rules previously managed that are no longer part of $M_{\text{target}}[\text{cat}]$ (i.e.,
$r \in M_{\text{prev}}[\text{cat}] \setminus M_{\text{target}}[\text{cat}]$) are automatically
retired and excluded from the final grants.

### 3.4 Combining Final Rules

The final grant list $\text{final}[\text{cat}]$ combines user custom rules and target managed rules,
preserving deterministic ordering:
$$\text{final}[\text{cat}] = U[\text{cat}] \cup M_{\text{target}}[\text{cat}]$$

---

## 4. Command Variant Expansion & Schema-Driven Hook Enforcement

[`lib/permissions.py`](../lib/permissions.py) and [`lib/agent_hooks/`](../lib/agent_hooks/__init__.py)
work together to match how agents invoke commands in the shell:

- **`allow` and `ask` grants (`literal=False`)**: Expanded into anchored `command(regex:...)`
  patterns so the agent runtime's `PermissionManager` can auto-approve commands invoked with
  leading environment variables, binary path prefixes, or tool global flags.
- **`deny` grants (`literal=True`)**: Emitted as canonical `command(<rule>)` strings (e.g.
  `command(git clean -f)`, `command(rm -rf /)`) and enforced before `PermissionManager` by the
  `PreToolUse` command hook using declarative `CommandSchema` definitions in
  [`lib/agent_hooks/schemas.py`](../lib/agent_hooks/schemas.py). Preceding `# Reason: <explanation>`
  comments in `never_allow.txt` are stored in `state.json` and surfaced in the hook's `deny` reason.

### 4.1 Environment Variable Prefixes

Commands frequently execute with leading environment variables (e.g. `GIT_PAGER=cat git status`).
`allow`/`ask` regexes prepend `ENV_VARS_PREFIX_PATTERN` to match leading `KEY=VALUE` assignments,
and the `PreToolUse` hook's `shell_lexer.strip_env_assignments` strips leading environment
assignments and command wrappers (`env`, `command`, `nice`, `nohup`, `time`, `stdbuf`) prior to
schema evaluation.

### 4.2 Git Global Flags

Git `allow`/`ask` regexes allow global flags before the subcommand (e.g. `git -C //src status`),
supporting `-C <dir>`, `--git-dir <dir>`, `--work-tree <dir>`, `--namespace <name>`, `--bare`,
`--no-pager`, `--no-color`, `--literal-pathspecs`, `--no-optional-locks`, `-p`, `--paginate`, and
`-c <config>`.

### 4.3 Fuchsia Tool Global Flags & Chaining (`fx`, `ffx`, `jiri`)

Fuchsia wrapper tools emit `allow`/`ask` regexes matching any binary path (`fx`, `scripts/fx`,
`/abs/path/fx`) and supported global flags:

- **`fx`**: Handles `-t <target>`, `--dir <out_dir>`, `--enable=...`, `--disable=...`,
  `--invoker=...`, `-x`, `-xx`, `-i`, `--`.
- **`ffx`**: Handles standalone `ffx` as well as chained `fx [flags] ffx [flags]`, including
  `--machine json`, `-t <target>`, `-c <config>`, `-v`, and `--isolate-dir`.
- **`jiri`**: Handles `-j <N>`, `-root <dir>`, `-color <mode>`, `-time`, `-v`, `-vv`, and
  `--show-progress`.

### 4.4 Schema-Driven `PreToolUse` Hook (`lib/agent_hooks`)

`fx agents setup` installs a managed `fuchsia-agents` `PreToolUse` hook in `~/.gemini/config/hooks.json`
that invokes [`lib/agent_hooks/pre_tool_use.py`](../lib/agent_hooks/pre_tool_use.py) on every
`run_command` tool call:

1. **Compound Command Decomposition**: `CommandRouter` splits compound shell lines (`&&`, `||`, `;`,
   `|`, `&`, and newlines) outside quotes and strips redirections and environment/wrapper prefixes
   from each subcommand.
2. **Declarative CLI Schemas**: `CommandRule` parses both the configured `deny` grants and each
   candidate subcommand against `CommandSchema` definitions in
   [`lib/agent_hooks/schemas.py`](../lib/agent_hooks/schemas.py) (`ffx`, `find`, `fx`, `git`, `grep`,
   `jiri`, `rm`, `sed`). This catches reordered flags (`git push origin main --force`), short-flag
   bundles (`git clean -fdx`, `rm -rfv /`), global flag prefixes (`git -C <dir> reset --hard`), and
   `fx ffx` delegation.
3. **In-Place `sed -i` Gating**: Because `read_only.txt` allows stream `sed`, `GrantPolicyConfig`
   checks whether an explicit `sed -i` grant is present in `config.json` (added by
   `local_changes.txt`). When `sed -i` is not granted (e.g. in `read-only` mode), the hook returns
   `"decision": "force_ask"` on any `sed` invocation supplying `-i` / `--in-place` (including
   bundled flags like `sed -Ei`).
4. **Opting Out (`--no-agent-hooks`)**: Passing `--no-agent-hooks` to `fx agents setup` removes the
   managed `fuchsia-agents` entry from `hooks.json` while keeping the `config.json` permission
   grants intact.

### 4.5 Unified System Binary Regex Expansion

For general binaries in `allow`/`ask` lists (e.g. `grep`, `jq`, `cat`, `ls`):

- Emits anchored regexes prepending `ENV_VARS_PREFIX_PATTERN` and optional binary path prefix
  `(\S+/)?`.
- Automatically permits standard system paths (`/bin/`, `/usr/bin/`), in-tree prebuilt toolchains
  (`prebuilt/...`), and bare commands via `$PATH`.
- Supports leading environment variables (e.g. `LC_ALL=C grep`) and subcommands without generating
  redundant duplicate rules.

---

## 5. State Tracking, Backups & Rollback

### 5.1 State File Schema

`fx agents setup` tracks its state in `~/.local/share/Fuchsia/agents/setup/state.json` (respecting
`$XDG_STATE_HOME` and `$XDG_DATA_HOME`). The state file records:

- Active profile name.
- Timestamp of last modification.
- Managed rule sets for each category (`allow`, `deny`, `ask`).
- History of applied profiles and corresponding backup snapshots.

### 5.2 Atomic Writes & Backups

- **Pre-Modification Backups**: Every write creates a timestamped backup in
  `~/.local/share/Fuchsia/agents/setup/backups/`, rolling and capped at `MAX_BACKUPS = 10`.
- **Atomic File Swaps**: Configuration updates (`config.json` and `hooks.json`) are written to
  `.{name}.tmp` and atomically swapped via `Path.replace` to prevent corrupted JSON on interruption.
- **Rollback (`--rollback [N]`)**: Reverts `config.json` to the state prior to N setup operations
  using the rolling backup snapshots and refreshes `hooks.json`.
- **Reset (`--reset`)**: Strips all Fuchsia-managed rules from `config.json`, removes the managed
  `fuchsia-agents` hook group from `hooks.json`, and clears `state.json` while preserving any
  developer-authored custom rules.

---

## 6. Rule Evaluation Precedence & Ad-Hoc Grants

### 6.1 Runtime Evaluation Precedence

When an AI coding agent executes a shell command, the `PreToolUse` hook (`hooks.json`) and the agent
runtime's `PermissionManager` (`config.json`) evaluate the command in priority order:

```text
                  Incoming Agent Command
                            │
                            ▼
        ┌───────────────────────────────────────┐
        │ PreToolUse Hook: Matches DENY rule?   │ ───► YES ───► [BLOCKED (Even in Turbo Mode)]
        └───────────────────┬───────────────────┘
                            │ NO
                            ▼
        ┌───────────────────────────────────────┐
        │ PreToolUse Hook: Ungranted `sed -i`?  │ ───► YES ───► [FORCE_ASK (Prompt in Normal Mode)]
        └───────────────────┬───────────────────┘
                            │ NO
                            ▼
        ┌───────────────────────────────────────┐
        │ PermissionManager: Matches ASK rule?  │ ───► YES ───► [PROMPT USER]
        └───────────────────┬───────────────────┘
                            │ NO
                            ▼
        ┌───────────────────────────────────────┐
        │ PermissionManager: Matches ALLOW rule?│ ───► YES ───► [AUTO-RUN]
        └───────────────────┬───────────────────┘
                            │ NO
                            ▼
                    [DEFAULT PROMPT]
```

- **`DENY` takes highest precedence**: If any subcommand in a compound command matches a `deny`
  grant in the `PreToolUse` hook, execution is rejected immediately across all profiles and modes
  (including `--dangerously-skip-permissions` Turbo mode).
- **`ASK` / `FORCE_ASK` takes precedence over `ALLOW`**: If a command triggers `force_ask` in the
  hook or matches an `ask` rule in `config.json` (and does not match `deny`), the user is prompted
  for interactive confirmation before execution.

### 6.2 Ad-Hoc Grants & Conflict Behavior

Developers can pass ad-hoc flags (`--allow`, `--deny`, `--ask`) or list files (`--allow-list`,
`--deny-list`, `--ask-list`) to customize any profile.

- **Combining Rules**: Ad-hoc `--allow` / `--ask` rules are expanded to regexes and ad-hoc `--deny`
  rules are emitted as literal `command(<rule>)` grants, appended to the target managed grant lists
  (`target_managed[cat]`) alongside the selected profile's rules.
- **Overlapping Rules in Configuration**:
  - `reconcile_grants` resolves conflicts between `target_managed` and `user_custom` rules (rules
    added to `config.json` manually outside of `fx agents`).
  - It does not prune contradictory rules within `target_managed` itself. If a user specifies
    `--deny "git commit"` while applying `local-changes` (which allows `git commit`), both the
    `allow` and `deny` rules are written to `config.json`.
  - Because `DENY` is enforced first by the `PreToolUse` hook, the command is blocked as intended.
- **Lifecycle & Re-runs**:
  - Ad-hoc flags apply to that specific `fx agents setup` run.
  - Running `fx agents setup` later without those flags re-applies the default profile manifests.
    If you want to maintain recurring custom restrictions, keep a custom list file (e.g.
    `my_denylist.txt`) and pass `--deny-list <path>` when running setup.
