# Planter machinery

This directory holds the shared rules that planter, an agent harness for
AI-assisted GN-to-Bazel migration, uses to migrate Fuchsia directories from GN
to Bazel. Everything here is read by the planter agents; nothing here
is part of the build.

*   [`machinery/`](machinery) is what the agents run with:
    *   `prompts/coder.md` and `prompts/playbooks/*.md`: instructions for the
        coding agent.
    *   `prompts/panel.json` and `prompts/reviewers/*.md`: the reviewer panel
        that checks every migration before upload.
    *   `checks/*.sh` (registered in `checks/manifest.json`): deterministic
        checks run on every pass. Each prints a JSON array of findings.
    *   `tools/*.sh` (registered in `tools/manifest.json`): helper tools the
        agents may call.
*   [`lessons/`](lessons) explains why the rules exist: one file per lesson,
    with the review or CQ failure that triggered it, the root cause, and why a
    one-off fix was not enough.

## How it changes

Planter learns from review. When a reviewer comments on a migration CL (or CQ
fails), planter diagnoses the general cause, updates `machinery/`, adds a file
to `lessons/`, and uploads that as a CL with the `planter-evolution` hashtag.
The engineer's own tasks use the change immediately; everyone else gets it once
the CL lands, because planter syncs this directory from `origin/main` before
each phase.

Keep reviews of `planter-evolution` CLs lightweight: because these files are
consumed by the agent rather than the build, a quick sanity check that new
rules or checks describe general GN/Bazel patterns (rather than workarounds
tied to specific directories) and contain nothing glaringly wrong is
sufficient. CLs that only touch `.md`, `lessons/`, or `machinery/` files can
include `No-Try: true` in the commit message to skip unnecessary CQ tryjobs.

Hand edits are welcome too; planter picks them up after they land.
