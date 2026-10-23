# Migration Package Selection

Select the **migration packages** one at a time:
1. Select the first **migration package** from the top of the **control table** and claim its ownership (steps 1 to 3).
2. After a package is added to the **migration packages**, also add the [packages that have to be migrated together](#packages-that-have-to-be-migrated-together) with it (step 4).
3. While the number of **migration packages** is below the **migration package count**, select another package and claim its ownership (steps 5, 2, 3 and 4). If the **migration package count** is 5 or less, the package must be able to be migrated in the same CL as the **migration packages** according to the [Grouping Rules](#grouping-rules). If it is more than 5, any package can be selected, because the **migration packages** are split into several **migration groups** in step 8.
4. Stop selecting when the number of **migration packages** reaches the **migration package count**, no more package can be selected, or a [selection failure](#selection-failures) happens. Then run steps 6 and 7 for each **migration package**, and step 8 to split the **migration packages** into **migration groups**.

The **migration package count** is checked only before selecting a new package. After a new package and the packages that have to be migrated together with it are added, the number of **migration packages** may exceed the **migration package count**; this is expected.

Migrating only the first **migration package** in one CL is the default and is expected when no other package can be grouped with it. If the **migration package count** is 5 or less, do not add a package to the CL only to reach the **migration package count**.

If there is a **migration directory**, follow [Directory Selection](#directory-selection) instead of steps 1 to 5, and then run steps 6 and 7 for each **migration package**, and step 8.

## Directory Selection

Select all the packages under the **migration directory** as the **migration packages**. A package is under the **migration directory** if its directory is the **migration directory** itself or one of its subdirectories, compared by whole path components (e.g., for the **migration directory** `src/lib/foo`, `src/lib/foo` and `src/lib/foo/bar` are under it, but `src/lib/foobar` is not).

1. Use the "gsheets" skill: In the **control table**, find all the rows whose package is under the **migration directory**.
   * If the **control table** link cannot be resolved or the sheet cannot be read, fail the Migration Package Selection step with the reason: "Failed to read the control table."
   * If no row is found, fail the Migration Package Selection step with the reason: "No package under `<migration directory>` is found in the control table."
2. From these rows, take the rows where the "Migration Status" column is "ready_for_migration" and the "Owner" column is empty as the candidate packages. Skip the other rows (e.g., packages that are already migrated, in progress, blocked, or owned by another user).
   * If there is no candidate package, fail the Migration Package Selection step with the reason: "No package under `<migration directory>` is ready for migration."
3. If there are more than 10 candidate packages, confirm with the user before claiming any of them, because all of them are migrated in this run. Tell the user the **migration directory**, the total number of candidate packages, and their directories, and ask whether to proceed with the migration of all of them.
   * If the user confirms, continue to the next step.
   * Otherwise, fail the Migration Package Selection step with the reason: "The user canceled the migration of `<migration directory>`."
4. For each candidate package, from the top of the **control table**, claim its ownership and check for an existing migration CL as described in [Claim migration ownership](#2-claim-migration-ownership) and [Check for an existing migration CL](#3-check-for-an-existing-migration-cl).
   * If it's dropped in [Claim migration ownership](#2-claim-migration-ownership) or [Check for an existing migration CL](#3-check-for-an-existing-migration-cl), skip it and continue with the next candidate package, instead of going to [Select the next package](#5-select-the-next-package).
   * If it's added to the **migration packages**, continue with the next candidate package, instead of going to [Add the packages that have to be migrated together](#4-add-the-packages-that-have-to-be-migrated-together).
   * If a [selection failure](#selection-failures) happens, handle it as described there.
5. When all the candidate packages are handled:
   * If no **migration package** has been selected, fail the Migration Package Selection step with the reason: "No migration package is found under `<migration directory>`."
   * Otherwise, continue to [Check the Buganizer Tickets](#6-check-the-buganizer-tickets).

In Directory Selection, the **migration package count**, [Add the packages that have to be migrated together](#4-add-the-packages-that-have-to-be-migrated-together) and [Select the next package](#5-select-the-next-package) do not apply: all the packages under the **migration directory** that can be claimed are migrated, and no package outside the **migration directory** is added. If there are more than 5 **migration packages**, they are split into **migration groups** in step 8.

## Grouping Rules

Read and follow the CL scoping guidelines in [scoping_changelists.md](../../../../references/migration/scoping_changelists.md) to decide which packages can be migrated in the same CL. The rules below summarize them; if they differ, [scoping_changelists.md](../../../../references/migration/scoping_changelists.md) takes precedence.

When the **migration packages** are split into **migration groups** in step 8, apply the rules within each **migration group**: read "the **migration packages**" as "the packages of the **migration group**", and "the first **migration package**" as "the first package of the **migration group**".

A candidate package can be added to the **migration packages** only when all of the following hold:
1. **Same area:** It is in the same subsystem or area as the **migration packages** (e.g., under the same `//src/<area>/...` subtree), or in the same generic parent directory (e.g., `//sdk/lib`). `OWNERS` files are a useful indicator of areas and subtrees. Never combine directories from different areas, even if one depends on another.
2. **Same target type:** Its targets are of the same type as the targets of the **migration packages**. Inspect the `BUILD.gn` files to determine the target types. For example, do not combine a host tool with libraries, a FIDL library with non-FIDL targets, or C++ targets with Rust targets.
3. **`third_party/` separation:** If any of the packages is in a `third_party/` directory, all of them are in the same third-party project (e.g., a C library and its `rust/` bindings subdirectory). Never combine `third_party/` directories with first-party directories, or directories of different third-party projects.
4. **One grouping pattern:** Together with the first **migration package**, it matches one of the patterns below, and all the **migration packages** follow the same pattern. Do not combine patterns or add extra directories: once a package is grouped with the first **migration package** by pattern A, B or D, whether it was added in step 4 or step 5, stop and do not also add sibling directories (pattern C) in step 5. Apart from this, the [packages that have to be migrated together](#packages-that-have-to-be-migrated-together) added in step 4 are not considered when applying this rule (e.g., the direct test subdirectories of sibling packages grouped by pattern C).
   * **A. Sole same-area consumer:** `fx gn refs` shows that the candidate package is the only remaining GN consumer of the first **migration package** (or the other way around), and the consumer has minimal dependencies.
   * **B. Root target and its private helpers:** The candidate package is a small helper subdirectory that exists primarily to support the first **migration package** (or the other way around).
   * **C. Small cohesive sibling group:** The packages are sibling or child directories that form one logical library family, or simple sibling libraries in an area or generic parent directory, and have similar build structures.
   * **D. Direct test subdirectory:** The candidate package is a direct test subdirectory (e.g., `//<dir>/tests` or `//<dir>/test`) of the first **migration package** and exercises its targets.
5. **Not a general-purpose library of a single consumer:** Do not group a foundational or general-purpose library with one of its many consumers.
6. **Small CL:** A **migration group** has at most 5 packages, not counting the [packages that have to be migrated together](#packages-that-have-to-be-migrated-together).

### Packages That Have to Be Migrated Together

The packages that have to be migrated together with a **migration package** are the packages that are tightly coupled with it:
   * Its sole same-area GN consumer, as in pattern A.
   * Its private helper packages, as in pattern B.
   * Its direct test subdirectories, as in pattern D.

They must also meet the rules 1 to 3 and 5 above. Adding them is part of selecting the **migration package**, so it does not count as combining grouping patterns, and they are added even if the number of **migration packages** then exceeds the **migration package count**, or their **migration group** then has more than 5 packages.

## Selection Failures

A selection failure is an error that prevents finding, claiming, or checking a candidate package in steps 1 to 5 (e.g., the **control table** cannot be read or updated, or the existing migration CL check cannot be run). Losing the ownership to another user in step 2 or finding an existing migration CL in step 3 is not a selection failure; in those cases, drop the candidate package and continue as described in those steps.

When a selection failure happens:
   1. If there is a candidate package whose ownership was claimed in step 2 (the re-check showed the current user as the "Owner") and which has not been added to the **migration packages** in step 3, use the "gsheets" skill: In the **control table**, change "Migration Status" of the candidate package to "blocked", write the reason of the selection failure to "Failure Reason", and set "Last Status Update" to the **status timestamp**. If it fails to update the **control table**, ignore the failure. The candidate package is not added to the **migration packages**.
      * Do not change the row of a candidate package whose ownership was not claimed (e.g., the "Owner" column could not be updated or re-checked in step 2), because another user may own it. Do not change the rows of the **migration packages** either.
   2. Then:
      * If no **migration package** has been selected yet, fail the Migration Package Selection step with the reason of the selection failure.
      * Otherwise, do not fail the Migration Package Selection step. Stop selecting packages and continue to [Check the Buganizer Tickets](#6-check-the-buganizer-tickets) with the **migration packages** selected so far. Do not change the status of these **migration packages** to "blocked"; they are migrated as usual.

## 1. Find the first package

Use the "gsheets" skill: In the **control table**, find the first row from the top where the "Migration Status" column is "ready_for_migration" and the "Owner" column is empty.
   * If the **control table** link cannot be resolved or the sheet cannot be read, it's a [selection failure](#selection-failures) with the reason: "Failed to read the control table."
   * If no row meets the requirements, fail the Migration Package Selection step with the reason: "No migration package is found."
   * The row that meets the requirements is the candidate package.


## 2. Claim migration ownership

For the candidate package, update the "Owner" column to the current user to claim ownership.

Wait for 5 seconds and then re-check the row whether the "Owner" column is the current user.
   * If not, drop the candidate package. Refer to [Select the next package](#5-select-the-next-package) to find another package for the migration.
   * If the "Owner" column cannot be updated or re-checked, it's a [selection failure](#selection-failures) with the reason: "Failed to claim the ownership of the package."


## 3. Check for an existing migration CL

After the ownership of the candidate package is claimed, use the "fuchsia-gerrit-cli" skill to check whether the candidate package has already been migrated and a CL has been generated for it (e.g., search all CL states for changes whose subject contains `[bazel_migration]` and whose affected files include the `BUILD.gn` or `BUILD.bazel` of the candidate package directory).
   * If such a CL is found, use the "gsheets" skill to update the "Migration Status" column of the candidate package to "migrated" and set "Last Status Update" to the **status timestamp**. Drop the candidate package. Refer to [Select the next package](#5-select-the-next-package) to find another package for the migration.
   * If the check cannot be run (e.g., the "fuchsia-gerrit-cli" skill returns an error), it's a [selection failure](#selection-failures) with the reason: "Failed to check for an existing migration CL."
   * Otherwise, add the candidate package to the **migration packages** and continue to the next step. Once it's added, it's no longer a candidate package.


## 4. Add the packages that have to be migrated together

For the package just added to the **migration packages** in step 3, find the [packages that have to be migrated together](#packages-that-have-to-be-migrated-together) with it (e.g., with `fx gn refs` and by inspecting the `BUILD.gn` files). For each of them whose row in the **control table** has "ready_for_migration" in the "Migration Status" column and an empty "Owner" column, and is not already one of the **migration packages**:
   * Claim its ownership and check for an existing migration CL as described in [Claim migration ownership](#2-claim-migration-ownership) and [Check for an existing migration CL](#3-check-for-an-existing-migration-cl), with this package as the candidate package.
   * If it's dropped in step 2 or step 3, skip it and continue with the next one, instead of going to step 5.
   * If it's added to the **migration packages** in step 3, run this step for it too, so that the packages that have to be migrated together with it are also added. This step ends because a package that is already one of the **migration packages** is never handled again.
   * Add it even if the number of **migration packages** then exceeds the **migration package count**.

When all of them, including the packages added transitively, are handled, continue to the next step.


## 5. Select the next package

* If no **migration package** has been selected yet, refer to [Find the first package](#1-find-the-first-package) to find another package for the migration.
* If the number of **migration packages** has reached the **migration package count**, continue to the next step.
* Otherwise, use the "gsheets" skill: In the **control table**, find the rows where the "Migration Status" column is "ready_for_migration", the "Owner" column is empty, and the row is not already one of the **migration packages**.
   * If the **migration package count** is 5 or less, from the top, find the first of these rows that can be grouped with the **migration packages** according to the [Grouping Rules](#grouping-rules).
   * If the **migration package count** is more than 5, take the first of these rows from the top. It does not need to be able to be grouped with the **migration packages**; it is put into a **migration group** in step 8.
   * If the **control table** cannot be read, it's a [selection failure](#selection-failures) with the reason: "Failed to read the control table."
   * If such a row is found, it's the candidate package. Refer to [Claim migration ownership](#2-claim-migration-ownership) to claim it.
   * If no such row is found, stop selecting packages and continue to the next step with the **migration packages** selected so far.


## 6. Check the Buganizer Tickets

Each **migration package** has its own Buganizer ticket, even though several **migration packages** are migrated in the same CL.

For each **migration package**, if the "Buganizer Ticket" column is empty, refer to [Buganizer ticket template](../assets/buganizer_ticket_template.md) to create a Buganizer ticket for the migration of that package with "buganizer-cli" agent skill, and use the "gsheets" skill to write it to the "Buganizer Ticket" column of that package in the **control table**.
   * If it fails to create the ticket, use the "Parent ID" defined in the [Buganizer ticket template](../assets/buganizer_ticket_template.md) as the Buganizer ticket number of that package and write it to the "Buganizer Ticket" column.
   * If the "Buganizer Ticket" column is not empty, keep using that ticket for the package.

## 7. Update Control Table

Use the "gsheets" skill: In the **control table**, change "Migration Status" of each **migration package** to "in-progress" and set "Last Status Update" to the **status timestamp**.


## 8. Split into migration groups

Each **migration group** is migrated by one Planter task and uploaded as one CL. Every **migration package** is in exactly one **migration group**.

* If there are 5 or fewer **migration packages**, or the **migration package count** is 5 or less and there is no **migration directory**, all the **migration packages** form one **migration group**.
* Otherwise, split the **migration packages** according to the [Grouping Rules](#grouping-rules):
  1. Start a new **migration group** with the first **migration package**, in the order they were selected, that is not in a **migration group** yet. Add the [packages that have to be migrated together](#packages-that-have-to-be-migrated-together) with it that are **migration packages**, transitively.
  2. In the order they were selected, add each **migration package** that is not in a **migration group** yet and can be grouped with the packages of this **migration group** according to the [Grouping Rules](#grouping-rules), together with the **migration packages** that have to be migrated together with it, while the **migration group** has fewer than 5 packages.
  3. Repeat from 1 until every **migration package** is in a **migration group**. A **migration package** that cannot be grouped with any other package forms a **migration group** by itself.
