---
name: fx-worktree
description: >
  Add and remove Fuchsia worktrees, supporting parallel AI agent development.
  Use when claiming, listing, syncing, or releasing isolated Fuchsia worktrees
  or managing the physical worktree pool with `fx worktree`.
---

# `fx worktree` (Fuchsia Worktree Manager)

`fx worktree` is a CLI tool designed to manage parallel development worktrees
inside a Fuchsia checkout. It allows multiple AI agents or developers to compile
and test code concurrently without conflicting build directories or git states.

Unlike standard git worktrees, `fx worktree` transparently reuses pre-warmed
checkouts from a physical pool managed via `fx worktree pool`.

## Everyday Agent Workflow

AI agents and developers interact with `fx worktree` using conventions matching
`git worktree add/remove/list`:

```bash
# Claim a worktree slot from the pool for your task (automatically syncs)
fx worktree add <task-name>

# Work inside the allocated directory alias (.jiri_root/worktrees/<task-name>
# unless the pool directory is configured elsewhere)
cd "$(fx worktree locate <task-name>)"

# When work is submitted or complete, return the checkout to the pool
fx worktree remove <task-name>
```

## CLI Commands

### Everyday Task Operations (`fx worktree`)

- **List active checkouts**:

  ```bash
  fx worktree list [--json]
  ```

  Lists all currently active (leased) checkouts, their task identifiers, sync
  status relative to the main checkout, and configured build directories. Pass
  `--json` to output structured JSON.

- **Claim a worktree**:

  ```bash
  fx worktree add <name> [--pool-name <slot>] [--json]
  ```

  Claims an available free slot from the pool for task `<name>` (or allocates
  the specific physical slot `<slot>` when `--pool-name` is specified),
  synchronizes the worktree with the parent checkout, backs up GN args, and
  creates the relative directory symlink `.jiri_root/worktrees/<name>`. Pass
  `--json` to output JSON with `worktree_id` and `path`.

- **Release a worktree**:

  ```bash
  fx worktree remove <name>
  ```

  Releases active worktree `<name>`, detaches git HEAD, restores backed-up GN
  args, and unlinks the symlink alias. Note: `<name>` must be the active task
  identifier (not the backing physical slot name).

- **Locate a worktree**:

  ```bash
  fx worktree locate <name>
  ```

  Prints the absolute filesystem path to worktree `<name>`. Note: `<name>` can
  be either the active task identifier or the backing physical slot name.

### Pool Capacity Management (`fx worktree pool`)

These administrative commands manage physical checkouts on disk under
`.jiri_root/worktrees/`. Users can move the pool elsewhere with
`jiri init -worktrees-dir=<path>` (relative to the checkout root, e.g.
`../fuchsia-worktrees`); paths below assume the default.

- **List pool inventory**:

  ```bash
  fx worktree pool list [--json]
  ```

  Lists all physical storage slots in the pool (both free and leased), active
  task tags, sync status, and configured build directories. Pass `--json` to
  output structured JSON.

- **Provision additional slot**:

  ```bash
  fx worktree pool add [name] [--set <args>] [--symlink-local | --copy-local]
  ```

  Provisions a new physical checkout in `.jiri_root/worktrees/<name>`. If `name`
  is omitted, automatically generates an ergonomic random identifier (e.g.
  `calm-meadow`).

  - `--set <args>`: Runs `fx set <args>` in the newly provisioned worktree (can
    be passed multiple times to pre-configure multiple build directories).
  - `--symlink-local`: Symlinks the `local` directory from the main checkout
    into the worktree.
  - `--copy-local`: Copies the `local` directory from the main checkout into the
    worktree.

  Note: `fx worktree add` automatically provisions a new slot if no free slot is
  available, so `pool add` is mainly used when pre-provisioning slots with
  specific build configurations or `local/` directories.

- **Decommission slot**:

  ```bash
  fx worktree pool remove <name> [--force]
  ```

  Permanently deletes a physical checkout directory from disk. Note: strictly
  requires `<name>` to be the literal directory name of a physical slot (cannot
  remove by task alias). If the slot is currently leased, `--force` is required.
