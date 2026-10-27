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
# LINT.ThenChange(//build/tools/bazel2gn/sentinel.go:bazel2gn_output)

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

    # Include deleted files, e.g. a deleted BUILD.bazel next to a remaining
    # bazel2gn-managed BUILD.gn must fail the check.
    affected_build_files = ctx.scm.affected_files(glob = _BUILD_FILES_GLOB, include_deleted = True)
    if not affected_build_files:
        return []

    gn_files = ctx.scm.all_files(glob = ["BUILD.gn"])

    dirs = {}

    for path in affected_build_files:
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
        "--json",
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
    if res.retcode == 0:
        return

    # bazel2gn prints no diagnostics if it fails before syncing any directory,
    # e.g. due to a bad flag. A non-zero exit must never pass silently.
    diagnostics = json.decode(res.stdout, default = None)
    if not diagnostics:
        error_msg = res.stderr.strip() or res.stdout.strip() or "no error message"
        ctx.emit.finding(
            level = "error",
            message = "bazel2gn failed: " + error_msg,
        )
        return
    _emit_bazel2gn_findings(ctx, diagnostics)

# LINT.IfChange(bazel2gn_json)
def _emit_bazel2gn_findings(ctx, diagnostics):
    """Emits findings for the diagnostics printed by `bazel2gn --json`."""
    out_of_sync_dirs = [diag["directory"] for diag in diagnostics if diag.get("out_of_sync")]
    fix_cmd = "fx bazel2gn " + " ".join(["-d " + d for d in out_of_sync_dirs])
    for diag in diagnostics:
        if diag.get("out_of_sync"):
            message = (
                "This file is out of sync with BUILD.bazel. Update BUILD.bazel " +
                "instead of editing below the BAZEL2GN SENTINEL, then sync " +
                "all out-of-sync directories with:\n  %s\n\n%s"
            ) % (fix_cmd, diag["message"])
        else:
            # The recipe that runs shac only shows the first line of each
            # finding in the build summary, so the error must come first.
            message = "bazel2gn failed: " + diag["message"]

        filepath = diag.get("file", "")
        line = diag.get("line", 0)

        # Paths are relative to the Fuchsia root, which may not be the root of
        # this repository (e.g. for vendor/google), and shac rejects findings
        # on files it doesn't know about.
        if filepath and not ctx.scm.all_files(glob = "/" + filepath):
            message = "%s:%s: %s" % (filepath, line, message) if line else "%s: %s" % (filepath, message)
            filepath, line = "", 0

        ctx.emit.finding(level = "error", filepath = filepath, line = line, message = message)

# LINT.ThenChange(//build/tools/bazel2gn/cmd/main.go:bazel2gn_json)

def register_bazel_migration_checks():
    shac.register_check(shac.check(_enforce_bazel_build_file_for_new_fidl_library))
    shac.register_check(shac.check(_verify_bazel2gn))
