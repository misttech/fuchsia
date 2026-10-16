# Bazel Migration and CL Creation

## Migration, CL Creation and Status Update

The `<package_name>` is the GN label of the **migration package**.

1. Record the current commit with `git rev-parse HEAD`.

2. From the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh` with parameters below to migrate the **migration package**.
   * `FUCHSIA_DIR`: The root of the Fuchsia checkout on the **migration branch**. Always set it, because the script otherwise defaults to `$HOME/fuchsia`, which may be a different checkout.
   * `--target-dir`: The directory of the **migration package**.
   * `--title`: Refer to **commit message guidelines** to create the title for this migration and provide the created title here.
   * `--task-id`: The components of `--target-dir` joined by `_` (e.g. the task-id of `src/connectivity/bluetooth/lib/bt-obex/objects` is `src_connectivity_bluetooth_lib_bt-obex_objects`).
   * `--desc`: Refer to **commit message guidelines** to create the commit message body for this migration and provide the created message body here.
     * Put the `Bug:` and `Bazel-Migration-Target:` footers in the last paragraph of `--desc`. Use the **buganizer ticket number** as the `Bug:` footer.
     * Leave out the `Test:` footer, because Planter adds its own test footers.
   * If the script returns error, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate `<package_name>`."

3. Run `git log -1 --format=%B` to get the commit message of the commit Planter just made, and then use "fuchsia-gerrit-cli" skill to get the CL number with the `Change-Id` in the retrieved commit message.
   * If it fails to get the CL number, fail the Bazel Migration and CL Creation step with the reason: "Fail to get the generated CL for `<package_name>` migration."

4. Review the commit message retrieved at the previous step based on the **commit message guidelines**. If there are review errors, fix them, amend the commit with updated commit message but keep the `Change-Id:` and Planter's `TAG:`, and run `git push origin HEAD:refs/for/main` to upload a new patchset.
   * If it fails to amend the commit or upload a new patchset, ignore the failure.

5. Use "gsheets" skill: Update the "Migration Status" of **migration package** in the **control table** to "CL_created" and set "Last Status Update" to the **status timestamp**.
   * If it fails to update the **control table**, ignore the failure.
