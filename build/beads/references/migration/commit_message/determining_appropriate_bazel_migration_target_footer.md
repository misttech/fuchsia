# Determining Appropriate `Bazel-Migration-Target:` Footers for Bazel Migration CLs

This document defines guidelines for selecting and formatting `Bazel-Migration-Target:` footers in Git commit messages for GN-to-Bazel migration changelists (CLs).

---

## 1. Overview

Every migration CL must include one or more `Bazel-Migration-Target:` footers to record the targets migrated in the CL so they can be queried in Gerrit (e.g., `hasfooter:Bazel-Migration-Target` or `footer:Bazel-Migration-Target=...`). Multiple `Bazel-Migration-Target:` lines may be specified when multiple targets are migrated.

- **Single line per target (no line wrapping):** `Bazel-Migration-Target:` lines may exceed the 72-character commit message line limit in order to fit the entire label. They **MUST NOT** be broken across multiple lines.
- **Alphabetical order:** When multiple `Bazel-Migration-Target:` lines are specified, list them in alphabetical order.

**Example:**
```none
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice_test
```

---

## 2. Selecting Target Values

**TODO(https://fxbug.dev/569007630):** Define the exact rules for selecting the best `Bazel-Migration-Target:` value(s) to specify.
