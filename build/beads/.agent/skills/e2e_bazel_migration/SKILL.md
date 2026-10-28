---
name: e2e-bazel-migration
description: >
  Performs an end-to-end migration of GN packages to Bazel using Planter,
  covering pre-migration checks, workspace branching, migration package
  selection from the control table, Buganizer ticket creation, package
  migration/verification, CL creation, review, and status reporting. Use this
  skill whenever the user asks to migrate targets using Planter (e.g., "migrate
  X using planter", "migrate the next X targets", "migrate targets in <dir>
  using planter"), run the automated GN-to-Bazel package migration workflow, or
  migrate packages from the control table.
---

## Terminology

* **control table** : The Google Sheet for tracking migration status, at
  go/fuchsia-bazel-migration-control-table. Use the `go-links` skill to resolve
  it to the sheet URL before using the `gsheets` skill. If the user specifies a
  different table in the prompt, use that one instead and do not resolve the
  link.

* **migration branch** : The branch used for the Bazel migration. If the user
  does not specify a branch, it defaults to `migration_branch_default`.

* **batching by default** : The policy of grouping as many cohesive
  packages/targets as make sense into a single CL (typically up to 5 packages
  from the same area and target type) to minimize the total number of CLs
  created per engineer, decrease reviewer burden, and preserve CQ capacity. A
  single-target CL is used only when no other cohesive ready packages can be
  grouped with it under the grouping rules, or when the user explicitly asks to
  migrate a single package (e.g., "migrate one package").

* **migration package count** : The number of **migration packages** in this
  migration. If the user specifies the number of packages in the prompt (e.g.,
  "migrate one package" means 1, "migrate three packages" means 3, "migrate the
  next 4 targets in //src/foo" means 4), use that number as the target or
  maximum package count. If the user does not specify a count (e.g., "migrate
  ready targets", "migrate the next ready targets", or "migrate packages under
  //src/lib/foo"), the default is **batching by default**: select as many
  cohesive packages as make sense to migrate in one CL (up to 5 packages of the
  same area and target type according to the grouping rules). When more than one
  package is selected at the [Migration Package
  Selection](#migration-package-selection) step, it is the total number of
  selected packages, including the first one, not the number of additional
  packages. It is checked only before selecting a new package: the packages that
  have to be migrated together with a selected package are added even if the
  number of **migration packages** then exceeds it. When there is a **migration
  directory** and a count is specified, it caps the candidate packages selected
  under that directory.

* **migration directory** : The directory the user asks to migrate in the prompt
  (e.g., "migrate all the packages under `//src/lib/foo`", "migrate the next 3
  targets in `src/connectivity/wlan`"), relative to the root of the Fuchsia
  checkout, without a leading `//` or a trailing `/` (e.g., `src/lib/foo`). If
  the user does not specify a directory, there is no **migration directory**.

* **migration packages** : The packages selected at the [Migration Package
  Selection](#migration-package-selection) step: the first package found in the
  **control table** and, up to the **migration package count**, more packages.
  By default, they are closely related packages of the same area and target type
  that can be migrated together in the same CL (up to 5 packages). If there is a
  **migration directory**, they are the candidate packages under it that can be
  claimed (up to the **migration package count** if specified). Each **migration
  package** is one row of the **control table**.

* **migration groups** : The groups the **migration packages** are split into at
  the [Migration Package Selection](#migration-package-selection) step,
  according to the grouping rules. The targets in the packages of a **migration
  group** and their package internal dependencies are migrated together by one
  Planter task and uploaded as one CL. If there are 5 or fewer **migration
  packages**, they are kept in one single **migration group** so they produce
  only one CL. If there are more than 5 **migration packages**, they are split
  into as few **migration groups** as possible (up to 5 packages per group),
  which are migrated in parallel by the Planter fleet, one CL for each
  **migration group**.

* **commit message guidelines** : The guidelines in this
  [migration_commit_message_guidelines.md](../../../references/migration/commit_message/guidelines.md)
  file.

* **buganizer ticket number** : For a **migration package**, the number in the
  "Buganizer Ticket" column of its row in the **control table**. Each
  **migration package** has its own ticket. If it's a URL, strip the
  "http://b/". (e.g., "http://b/1234567", the buganizer ticket number is
  "1234567")

* **status timestamp** : The current time in UTC, formatted as `YYYY-MM-DD HH:MM
  UTC` (e.g., `2026-10-16 04:31 UTC`).

* **auth check command** : The command in the "Command" section of the document
  at go/auth_check. It exits non-zero if the user's credentials expire within
  one hour. If the user specifies a different command in the prompt, use that
  one instead.

* **assign reviewer** : Whether Planter assigns a human reviewer to each created
  CL with `--assign-reviewer`, once a CQ dry run of the CL passes. Planter picks
  the reviewer by the reviewer routing table at
  go/fuchsia-bazel-migration-reviewers. It is enabled if the user asks for it in
  the prompt (e.g., "assign reviewers", "use --assign-reviewer"). Otherwise, it
  is disabled.

* **Planter check script** : The script in the "Script" section of the document
  at go/planter_check_and_update. It checks whether the installed Planter is up
  to date and updates it if it is not. If the user specifies a different script
  in the prompt, use that one instead.

* **Planter installation guide** : The document with the steps to install
  Planter, at go/planter_installation_guide. If the user specifies a different
  guide in the prompt, use that one instead.

* **Planter tag** : The `TAG: planter planter-<freshness>` footer that Planter
  writes into the commit message of every CL it creates (e.g.,
  `TAG: planter planter-20261008-633b8b4b94a`). `<freshness>` identifies the
  Planter machinery that generated the CL. Only Planter knows it.

## Planter Rules

These rules apply to every step of this skill, and to any later work on a
migration CL (e.g., addressing review comments, fixing CQ failures, rebasing
or re-uploading).

1. **Preserve the Planter tag. Never invent one.**
   * Planter generates the commit message of every migration CL. Commit and
     upload migration CLs with Planter (`scripts/planter_task_add_run.sh`,
     `planter run --gerrit` or `planter fleet --gerrit`); do not create the
     commits of new migration CLs yourself.
   * When you amend a commit message of a migration CL, keep the **Planter
     tag** exactly as Planter wrote it: copy the line verbatim from the current
     commit message (`git log -1 --format=%B`) or from the CL on Gerrit. Do not
     remove it, move it out of the footer block, or change any character of it.
     It goes right before the `Change-Id:` line.
   * Never write a `TAG:` line yourself: do not make up a `<freshness>` value,
     copy one from another CL, or add a `TAG:` line to get past a check. If a
     migration CL has no **Planter tag**, leave it without one and tell the user
     that the CL has no **Planter tag** and was probably not generated or
     uploaded by Planter.

## End-to-End Bazel Migration

If any step fails, refer to [Error Handling](#error-handling) to handle the
error condition.

### Pre-Migration Checks

Refer to `references/pre_migration_checks.md` to verify if the required criteria
for the Bazel migration are fulfilled.

  * If this step fails, it's a **STOP** condition.

### Workspace Branching

Refer to `references/workspace_branching.md` to ensure you are on the
**migration branch**.

  * If this step fails, it's a **STOP** condition.

### Migration Package Selection

Refer to `references/migration_package_selection.md` to select the **migration
packages** and split them into **migration groups**.

  * If this step fails, it's a **STOP** condition.

### Bazel Migration and CL Creation

Refer to `references/bazel_migration_planter.md` to migrate the targets within
the **migration packages**, one CL for each **migration group**.

  * If this step fails because it failed to get the generated CL number, or
    because no CL is created for any of several **migration groups**, it's a
    **STOP** condition.
  * If this step fails because of other reasons, it's a **PLANTER ERROR**
    condition.

## CL Review

Refer to `references/cl_review.md` to review each created CL and update result
to it.

## Error Handling

Refer to `references/error_handling.md` to handle the reported error condition.

## Notifications

Refer to `references/notifications.md` to notify user.
