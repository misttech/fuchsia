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

* **control table** : The Google Sheet for tracking migration status. The sheet
  is at
  https://docs.google.com/spreadsheets/d/1LJBype1IvOKlCrNd2NQdpEEnDMvSCZYd7e4wVp29gOg/edit?resourcekey=0-_guvG1cqt62s4cBY-aeuiQ&gid=0#gid=0
  .

* **migration branch** : The branch used for the Bazel migration. If the user
  does not specify a branch, it defaults to `migration_branch_default`.

* **migration package** : The package selected at the [Migration Package
  Selection](#migration-package-selection) step. The targets in this package and
  package internal dependencies will be migrated together.

* **commit message guidelines** : The guidelines in this
  [migration_commit_message_guidelines.md](../../../references/migration/commit_message/guidelines.md)
  file.

* **buganizer ticket number** : The number in the "Buganizer Ticket" column of
  the row for the **migration package** in the **control table**. If it's a URL,
  strip the "http://b/". (e.g., "http://b/1234567", the buganizer ticket number
  is "1234567")

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
package**.
  * If this step fails, it's a **STOP** condition.

### Bazel Migration and CL Creation

Refer to `references/bazel_migration_planter.md` to migrate the targets within
the **migration package**.
  * If this step fails because it failed to get the generated CL number, it's a
    **STOP** condition.
  * If this step fails because of other reasons, it's a **PLANTER ERROR**
    condition.

## CL Review

Refer to `references/cl_review.md` to review the CL and update result to it.

## Error Handling

Refer to `references/error_handling.md` to handle the reported error condition.

## Notifications

Refer to `references/notifications.md` to notify user.
