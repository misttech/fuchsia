# Jj colocated change base commit message resolution

- **Learned from:** inner loop friction on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 1 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/commit_message_format.sh`

## Root cause

In checks/commit_message_format.sh, the git() helper ignored subprocess return
codes and did not pass --verify to git rev-parse. When PLANTER_CHANGE_BASE was
set to a Jujutsu revset such as @- in a jj colocated checkout, git rev-parse @-
exited with status 128 while echoing '@-' to stdout, so base_rev became the
literal string '@-' instead of the git commit SHA of @- (which equals git HEAD
in a colocated checkout). Consequently, head != base_rev evaluated to true and
commit_message_format linted the unrelated upstream commit at git HEAD.

## Why not a one-off fix

Suppressing or working around commit_message_format findings for a single
package would leave commit_message_format broken for every migration run in a jj
colocated workspace where PLANTER_CHANGE_BASE is a jj revset such as @-.
Resolving revisions with git rev-parse --verify, checking git exit codes, and
falling back to jj log --ignore-working-copy fixes revision resolution across
all packages.
