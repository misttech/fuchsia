---
name: e2e-bazel-migration
description: >
  Performs an end-to-end migration of GN packages to Bazel, covering pre-
  migration checks, workspace branching, migration package selection,
  Buganizer ticket creation, package migration/verification, CL creation,
  review and status reporting. Use this skill when the user wants to run the
  full automated GN-to-Bazel package migration workflow or migrate a batch of
  packages from start to finish.
---

## Terminology

* **control table** : The Google Sheet for tracking migration status, at
  go/fuchsia-bazel-migration-control-table. Use the `go-links` skill to resolve
  it to the sheet URL before using the `gsheets` skill. If the user specifies a
  different table in the prompt, use that one instead and do not resolve the
  link.

* **migration branch** : The branch used for the Bazel migration. If the user
  does not specify a branch, it defaults to `migration_branch_default`.

* **migration package count** : The number of **migration packages** in this
  migration. If the user specifies the number of packages in the prompt (e.g.,
  "migrate one package" means 1, "migrate three packages" means 3), use that
  number. If the user does not specify it, it defaults to 1. When more than one
  package is selected at the [Migration Package
  Selection](#migration-package-selection) step, it is the total number of
  selected packages, including the first one, not the number of additional
  packages. It is checked only before selecting a new package: the packages
  that have to be migrated together with a selected package are added even if
  the number of **migration packages** then exceeds it. It does not apply when
  there is a **migration directory**.

* **migration directory** : The directory the user asks to migrate in the
  prompt (e.g., "migrate all the packages under `//src/lib/foo`"), relative to
  the root of the Fuchsia checkout, without a leading `//` or a trailing `/`
  (e.g., `src/lib/foo`). If the user does not specify a directory, there is no
  **migration directory**.

* **migration packages** : The packages selected at the [Migration Package
  Selection](#migration-package-selection) step: the first package found in the
  **control table** and, up to the **migration package count**, more packages.
  If the **migration package count** is 5 or less, they are closely related
  packages of the same area and target type that can be migrated in the same
  CL. If there is a **migration directory**, they are all the packages under it
  that can be claimed instead. Each **migration package** is one row of the
  **control table**.

* **migration groups** : The groups the **migration packages** are split into
  at the [Migration Package Selection](#migration-package-selection) step,
  according to the grouping rules. The targets in the packages of a
  **migration group** and their package internal dependencies are migrated
  together by one Planter task and uploaded as one CL. If there are more than
  5 **migration packages**, they are usually split into several **migration
  groups**, which are migrated in parallel by the Planter fleet, one CL for
  each **migration group**. Otherwise, there is one **migration group**.

* **commit message guidelines** : The guidelines in this
  [migration_commit_message_guidelines.md](../../../references/migration/commit_message/guidelines.md)
  file.

* **buganizer ticket number** : For a **migration package**, the number in the
  "Buganizer Ticket" column of its row in the **control table**. Each
  **migration package** has its own ticket. If it's a URL, strip the
  "http://b/". (e.g.,
  "http://b/1234567", the buganizer ticket number is "1234567")

* **status timestamp** : The current time in UTC, formatted as
  `YYYY-MM-DD HH:MM UTC` (e.g., `2026-10-16 04:31 UTC`).

* **auth check command** : The command in the "Command" section of the
  document at go/auth_check. It exits non-zero if the user's credentials expire
  within one hour. If the user specifies a different command in the prompt, use
  that one instead.

* **Planter installation guide** : The document with the steps to install
  Planter, at go/planter_installation_guide. If the user specifies a different
  guide in the prompt, use that one instead.

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
