# Pre-Migration Checks

Run the checks to verify if the required criteria for the Bazel migration are fulfilled.

## Required Skills Check

The Bazel migration uses the skills below.

| Skill | Used for | Location |
| :---- | :------- | :------- |
| `gmail` | Sending notifications | Use the `skill_search` tool to find it |
| `gsheets` | Reading and updating the **control table** | Use the `skill_search` tool to find it |
| `fuchsia-gerrit-cli` | Getting the CL number and posting CL review results | `//.agents/skills/fuchsia-gerrit-cli` |
| `buganizer-cli` | Creating the Buganizer ticket | Use the `skill_search` tool to find it |
| `go-links` | Resolving go links | Use the `skill_search` tool to find it |
| `fx-worktree` | Allocating worktree slots for the Planter fleet | `//.agents/skills/fx-worktree` |

`//` is the root of the Fuchsia checkout.

### 1. Check the Installed Skills

1. For each skill above, check whether it is listed in your available skills.
   * If all the skills are listed, the check passes.
   * If any skill is not listed, refer to [Install the Missing Skills](#2-install-the-missing-skills) to install it.

### 2. Install the Missing Skills

1. For each missing skill, get the directory that contains its `SKILL.md` from the "Location" column above.
   * If the location is a directory in the Fuchsia checkout (e.g. `//.agents/skills/fx-worktree`), use that directory. Check that its `SKILL.md` exists and has the same skill name.
   * Otherwise, use the `skill_search` tool to search for it by name. The search result must have the same skill name (e.g. `buganizer-cli`).
   * If the skill is not found, fail the Pre-Migration Checks step with the reason: "Failed to find the `<skill>` skill".

2. Register the skill in `~/.gemini/config/skills.json` so that it is available in later sessions.
   * Add an entry whose `path` is the parent directory of the skill directory and whose `include_only` lists the skill directory name (e.g. `{"path": "<skills_parent_dir>", "include_only": ["<skill_dir_name>"]}`). The skill directory name can differ from the skill name (e.g. directory `buganizer_cli` for skill `buganizer-cli`). If an entry with the same `path` already exists, add the skill directory name to its `include_only` instead.
   * Create the file with `{"entries": []}` if it does not exist. Keep all existing entries.
   * If it fails to update the file, fail the Pre-Migration Checks step with the reason: "Failed to install the `<skill>` skill".

3. Newly registered skills may not be listed until a new session starts. For the current run, read the `SKILL.md` of each installed skill from the directory found in step 1, and follow it when this skill refers to that skill.


## Authentication Check

1. Use the `go-links` skill to resolve the **auth check command** link to its destination, read the document it points to, and get the command from its "Command" section.
   * If the user specified the auth check command in the prompt, skip this step and use that command.
   * If the link cannot be resolved, the document cannot be read, or it has no command, fail the Pre-Migration Checks step with the reason: "Failed to get the auth check command".

2. Run the **auth check command**.
   * If it exits non-zero, fail the Pre-Migration Checks step with the reason: "Credentials expired or will expire in an hour".


## Planter Installation Check

The `<planter>` below is the `planter` binary resolved with `"$(command -v planter || echo "$HOME/.local/bin/planter")"`, the same way `scripts/planter_task_add_run.sh` finds it. Use it instead of a bare `planter`, because `~/.local/bin` may not be on `PATH`.


### 1. Check the Planter Binary

1. Run `command -v planter || ls -l "$HOME/.local/bin/planter"` to find the `planter` binary.
   * If the binary is found, run `<planter> version` to verify that it starts.
   * If the binary is not found, or `<planter> version` fails, refer to [Install Planter](#2-install-planter) to install it.


### 2. Install Planter

1. Use the `go-links` skill to resolve the **Planter installation guide** link to its destination, read the document it points to, and follow its steps to install the `planter` prebuilt.
   * If the user specified a different Planter installation guide in the prompt, do not resolve the link. Read that guide instead and follow its steps.
   * If the link cannot be resolved or the document cannot be read, fail the Pre-Migration Checks step with the reason: "Failed to read the Planter installation guide".
   * The build can take 10 minutes or more. Allow a long timeout or run the command in the background and wait for it to finish; do not treat a slow build as a failure.

2. Run `<planter> version` to verify the installation.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Failed to install Planter".
   * If `command -v planter` still finds nothing, `~/.local/bin` is not on `PATH`. This is fine for the migration: `scripts/planter_task_add_run.sh` falls back to `~/.local/bin/planter`.


### 3. Check the Shared Machinery

1. Run `ls "$HOME/.planter/beads/build/beads/planter/machinery/prompts/coder.md"` to check whether the Planter shared machinery is set up.
   * If the file does not exist, run `<planter> init-project` from the root of the Fuchsia checkout to set it up.
   * If `<planter> init-project` fails, fail the Pre-Migration Checks step with the reason: "Failed to initialize the Planter shared machinery".


### 4. Check the Antigravity Environment

Planter drives its coding and reviewer agents through the Antigravity language server. It reads the server address from `ANTIGRAVITY_LS_ADDRESS` (default `localhost:5387`) and the CSRF token from `ANTIGRAVITY_CSRF_TOKEN`, which are set in Antigravity terminals.

1. Run `[[ -n "${ANTIGRAVITY_CSRF_TOKEN:-}" ]]` to check that the command runs in an Antigravity terminal.
   * If it fails, fail the Pre-Migration Checks step with the reason: "Planter must run from an Antigravity terminal".
