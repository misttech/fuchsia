# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines SHAC checks for the GN-to-Bazel migration.

These checks are only needed until the migration from GN to Bazel is complete.
"""

load(
    "./common.star",
    "cipd_platform_name",
    "compiled_tool_path",
    "get_fuchsia_dir",
    "os_exec",
)

# LINT.IfChange(bazel2gn_output)
_BAZEL2GN_SENTINEL = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##"
_BATCH_ERROR_PREFIX = "* syncing directory "
_OUT_OF_SYNC_MESSAGE = "BUILD.gn is out of sync with BUILD.bazel"
# LINT.ThenChange(//build/tools/bazel2gn/cmd/main.go:bazel2gn_output)

# Changes to these files can change bazel2gn's output for any directory, so
# trigger a check of all bazel2gn-managed directories.
_BAZEL2GN_INPUTS_GLOB = [
    "/build/tools/bazel2gn/**",
    # Pins the gn and buildifier prebuilts.
    "/manifests/prebuilts",
    "/scripts/shac/bazel_migration.star",
]

# Patterns without slashes match at any level, including the root.
_BUILD_FILES_GLOB = ["BUILD.gn", "BUILD.bazel"]

def _enforce_bazel_build_file_for_new_fidl_library(ctx):
    """Checks that a BUILD.bazel file exists when a new FIDL library is created in GN."""

    # Check when :
    # 1. The file is a new BUILD.gn file added under //sdk/fidl/.
    # 2. The BUILD.gn file contains at least one fidl() template.
    for path, meta in ctx.scm.affected_files(glob = "BUILD.gn").items():
        if meta.action != "A":
            continue

        # TODO(https://fxbug.dev/487883318): Extend the check to FIDL libraries outside //sdk/fidl directory.
        if not path.startswith("sdk/fidl/"):
            continue

        # Read the BUILD.gn content
        content = str(ctx.io.read_file(ctx.scm.root + "/" + path))

        # Check if it contains at least one fidl() template
        has_fidl = False
        for line in content.splitlines():
            # Ignore comments
            hash_idx = line.find("#")
            if hash_idx != -1:
                line_code = line[:hash_idx]
            else:
                line_code = line
            if ctx.re.allmatches(r"\bfidl\s*\(\s*[\"\']", line_code):
                has_fidl = True
                break

        # Skip if the BUILD.gn file doesn't contain any fidl() template
        if not has_fidl:
            continue

        # Check if BUILD.bazel exists in the same directory
        bazel_path = "/".join(path.split("/")[:-1] + ["BUILD.bazel"])

        if not ctx.scm.all_files(glob = bazel_path):
            ctx.emit.finding(
                level = "error",
                filepath = path,
                message = "BUILD.bazel file is missing in the same directory as the added FIDL BUILD.gn file.",
            )

def _dirname(path):
    return path.rpartition("/")[0] or "."

def _dirs_to_check(ctx):
    """Returns the bazel2gn-managed directories to check, or None for all.

    Under `shac check --all`, every file is affected, including bazel2gn's
    inputs, so all directories are checked. That only takes a few seconds.
    """
    if ctx.scm.affected_files(glob = _BAZEL2GN_INPUTS_GLOB, include_deleted = True):
        return None

    gn_files = ctx.scm.all_files(glob = ["BUILD.gn"])

    dirs = {}

    # Include deleted files, e.g. a deleted BUILD.bazel next to a remaining
    # bazel2gn-managed BUILD.gn must fail the check.
    for path in ctx.scm.affected_files(glob = _BUILD_FILES_GLOB, include_deleted = True):
        # Check the BUILD.gn in the same directory, which also covers affected
        # BUILD.bazel files.
        d = _dirname(path)
        if d in dirs:
            continue
        gn_path = "BUILD.gn" if d == "." else d + "/BUILD.gn"
        if gn_path in gn_files and _BAZEL2GN_SENTINEL in str(ctx.io.read_file(ctx.scm.root + "/" + gn_path)):
            dirs[d] = True
    return sorted(dirs.keys())

def _verify_bazel2gn(ctx):
    """Verifies BUILD.gn files managed by bazel2gn are in sync with BUILD.bazel."""
    fuchsia_dir = get_fuchsia_dir(ctx)

    dirs = _dirs_to_check(ctx)
    if dirs == []:
        return

    platform = cipd_platform_name(ctx)
    cmd = [
        compiled_tool_path(ctx, "bazel2gn"),
        "--check_only",
        "--fuchsia_dir",
        fuchsia_dir,
        "--gn_bin",
        "%s/prebuilt/third_party/gn/%s/gn" % (fuchsia_dir, platform),
        "--buildifier_bin",
        "%s/prebuilt/third_party/buildifier/%s/buildifier" % (fuchsia_dir, platform),
    ]
    if dirs != None:
        # Without --directory_list, bazel2gn checks all bazel2gn-managed
        # directories.
        cmd += ["--directory_list", ctx.io.tempfile("\n".join(dirs) + "\n")]

    res = os_exec(ctx, cmd, ok_retcodes = (0, 1)).wait()
    if res.retcode != 0:
        _emit_bazel2gn_findings(ctx, res.stderr + "\n" + res.stdout)

def _emit_bazel2gn_findings(ctx, output):
    """Emits findings for a failed `bazel2gn --check_only` run."""

    # Per-directory errors, keyed by directory.
    errors = {}
    for chunk in ("\n" + output).split("\n" + _BATCH_ERROR_PREFIX)[1:]:
        d, _, err = chunk.partition(": ")
        errors[d] = err.strip()

    out_of_sync = sorted([d for d, err in errors.items() if err.startswith(_OUT_OF_SYNC_MESSAGE)])
    fix_cmd = "fx bazel2gn " + " ".join(["-d " + d for d in out_of_sync])
    for d in out_of_sync:
        ctx.emit.finding(
            level = "error",
            filepath = "BUILD.gn" if d == "." else d + "/BUILD.gn",
            message = (
                "This file is out of sync with BUILD.bazel. Update BUILD.bazel " +
                "instead of editing below the BAZEL2GN SENTINEL, then sync " +
                "all out-of-sync directories with:\n  %s\n\n%s"
            ) % (fix_cmd, errors[d][len(_OUT_OF_SYNC_MESSAGE):].strip()),
        )

    # Any other errors, e.g. a BUILD.bazel that fails to parse. A non-zero exit
    # must never pass silently.
    other_errors = [
        _BATCH_ERROR_PREFIX + "%s: %s" % (d, err)
        for d, err in errors.items()
        if d not in out_of_sync
    ]
    if other_errors or not out_of_sync:
        ctx.emit.finding(
            level = "error",
            message = "bazel2gn failed:\n" + ("\n".join(other_errors) or output.strip()),
        )

def register_bazel_migration_checks():
    shac.register_check(shac.check(_enforce_bazel_build_file_for_new_fidl_library))
    shac.register_check(shac.check(_verify_bazel2gn))
