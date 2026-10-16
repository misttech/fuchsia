# CL Review

Perform a code review of the migration CL using the common and target-specific review checklists.

## 1. Common Review Checks

Review the CL against the universal criteria in the common checklist:
* Refer to [Common CL Review Checklist](checklists/cl_review_common_checklist.md).

## 2. Target-Specific Review Checks

Based on the migrated target type, review the changes against the corresponding checklist:

### Host Tools Targets:
* Refer to [Host Tool Migration CL Review Checklist](checklists/cl_review_host_tool_migration_checklist.md).

### FIDL Libraries (under `//sdk/fidl` or other directories):
* Refer to [FIDL Migration CL Review Checklist](checklists/cl_review_fidl_migration_checklist.md).

## 3. Update Status

1. After completing the review, refer to [Bazel Migration Review Result Template](../assets/review_result_template.md) to generate review result and use "fuchsia-gerrit-cli" skill to post the result to the reviewed CLs as resolved.

2. Use the "gsheets" skill: In the **control table**, write the URL of the reviewed CL (e.g., `https://fuchsia-review.googlesource.com/c/fuchsia/+/<CL number>`) to the "Fuchsia Gerrit CL" column of the **migration package**.
   * If it fails to update the **control table**, ignore the failure.
