# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC check enforcing a one-way presubmit ratchet against direct //prebuilt references."""

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

# LINT.IfChange
_EXCLUDED_PATHS = (
    "build/bazel/rules/cipd_prebuilt.bzl",
    "scripts/shac/bazel_lint/checks/tests/root_package_labels_test.star",
    "scripts/shac/prebuilts.star",
    "scripts/shac/tests/prebuilts_test.star",
)

_EXCLUDED_DIR_PREFIXES = (
    "build/prebuilt/",
    "third_party/",
    "docs/",
    "manifests/",
    "tools/check-licenses/assets/readmes/",
)

_EXCLUDED_BASENAMES = (
    "OWNERS",
    "LICENSE",
    "AUTHORS",
    "PATENTS",
    "README",
    "NOTICE",
    "COPYING",
    "CONTRIBUTORS",
    "CODEOWNERS",
)

_CHECKED_SUFFIXES = (
    ".gn",
    ".gni",
    ".bazel",
    ".bzl",
    ".fx",
    ".sh",
    ".bash",
    ".py",
    ".star",
    ".cmake",
    ".c",
    ".cc",
    ".cpp",
    ".h",
    ".hpp",
    ".rs",
    ".go",
    ".dart",
)

# LINT.ThenChange(//build/prebuilt/on_demand_migration/scripts/ratchet/languages.py)

def _is_checked_file(path):
    if path in _EXCLUDED_PATHS:
        return False
    for prefix in _EXCLUDED_DIR_PREFIXES:
        if path.startswith(prefix):
            return False
    if path.startswith("testdata/") or "/testdata/" in path:
        return False
    basename = path.rsplit("/", 1)[-1]
    if basename in _EXCLUDED_BASENAMES:
        return False
    if basename == "CMakeLists.txt":
        return True
    if "." not in basename:
        return True
    for suffix in _CHECKED_SUFFIXES:
        if basename.endswith(suffix):
            return True
    return False

def check_no_new_direct_prebuilts(ctx):
    """Enforces a one-way ratchet against direct //prebuilt references.

    Args:
      ctx: A SHAC ctx instance.
    """
    files = [
        path
        for path in ctx.scm.affected_files()
        if _is_checked_file(path)
    ]
    if not files:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        cipd_platform_name(ctx),
    )
    checker_script = (
        "%s/build/prebuilt/on_demand_migration/scripts/ratchet/ratchet.py" %
        fuchsia_dir
    )

    files_file = ctx.io.tempfile("\n".join(files) + "\n")
    completed = os_exec(
        ctx,
        [
            python_bin,
            checker_script,
            "--root",
            ctx.scm.root,
            "--json",
            "--files-from",
            files_file,
        ],
        ok_retcodes = (0, 1),
    ).wait()

    findings = json.decode(completed.stdout)
    if type(findings) != "list":
        fail("Expected a list of findings from %s (exit code %d), got: %s\nstderr: %s" % (
            checker_script,
            completed.retcode,
            completed.stdout,
            completed.stderr,
        ))

    if bool(completed.retcode) != bool(findings):
        fail("%s exited with code %d, but reported %d findings" % (
            checker_script,
            completed.retcode,
            len(findings),
        ))

    for finding in findings:
        ctx.emit.finding(
            level = finding.get("level", "error"),
            filepath = finding["filepath"],
            line = finding["line"],
            col = finding["col"],
            message = finding["message"],
        )

def register_prebuilt_checks():
    shac.register_check(shac.check(check_no_new_direct_prebuilts))
