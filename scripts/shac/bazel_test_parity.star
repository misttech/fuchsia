# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC check enforcing GN-to-Bazel test parity and loud test-deletion alerts."""

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

_RELEVANT_GLOBS = [
    "BUILD.gn",
    "BUILD.bazel",
    "BUILD",
    "/COMMIT_MSG",
    "*.gni",
    "*.bzl",
    "*test*",
    "**/*test*",
    "!third_party/bazel_vendor/**",
    "!third_party/rust_crates/vendor/**",
]

_PARITY_FOOTER_RE = r"(?im)^(Build-Only|Deleted-Tests|Skipped-Dual-Tests|Renamed-Tests)\s*:"

def _is_submitted_commit(message):
    """Returns True if a commit has already been submitted by Gerrit or a roller."""
    lines = message.rstrip().splitlines()
    last_para_start = 0
    for i in range(len(lines) - 1, -1, -1):
        if not lines[i].strip():
            last_para_start = i + 1
            break
    return any([
        line.startswith("Reviewed-on:") or line.startswith("Roller-URL:")
        for line in lines[last_para_start:]
    ])

def _bazel_test_parity(ctx):
    """Checks for silently orphaned or unattested deleted tests across GN and Bazel."""
    fuchsia_dir = get_fuchsia_dir(ctx)

    # Exclude repositories other than `fuchsia.git` (such as `vendor/*`), which
    # have separate workspace roots, `shac.textproto` schemas, and OOT driver
    # packaging conventions.
    if ctx.scm.root != fuchsia_dir:
        return

    all_live_files = set(ctx.scm.all_files().keys())
    all_affected = ctx.scm.affected_files(include_deleted = True)
    any_scm_action = any([bool(meta.action) for meta in all_affected.values()])

    # Distinguish `shac check <files>` (`specifiedFiles`, where `meta.action` is
    # always empty and `all_affected` holds only the requested subset) from
    # `shac check [--all]` (`gitCheckout`, where `--all` populates
    # `all_affected` with every live file in the repository).
    is_specified_files = (
        not any_scm_action and
        len(all_affected) > 0 and
        len(all_affected) < len(all_live_files)
    )

    commits = ctx.scm.commits()

    # Skip commits that have already been submitted (e.g. postsubmit CI or a
    # clean checkout at `JIRI_HEAD` / `origin/main`) unless specific files were
    # explicitly requested via `shac check <files>`.
    if not is_specified_files and commits and all([
        _is_submitted_commit(c.message)
        for c in commits
    ]):
        return

    affected = ctx.scm.affected_files(
        glob = _RELEVANT_GLOBS,
        include_deleted = True,
    )
    has_parity_footer = any([
        bool(ctx.re.match(_PARITY_FOOTER_RE, c.message))
        for c in commits
    ])

    modified = [path for path, meta in affected.items() if meta.action]
    if modified:
        files_to_pass = modified
    elif is_specified_files:
        if affected:
            files_to_pass = list(affected.keys())
        elif has_parity_footer:
            files_to_pass = []
        else:
            return
    elif has_parity_footer:
        files_to_pass = []
    else:
        return

    platform = cipd_platform_name(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        platform,
    )
    local_checker = "%s/scripts/shac/check_bazel_test_parity.py" % ctx.scm.root
    checker_script = (
        local_checker if ctx.io.stat(local_checker) else "%s/scripts/shac/check_bazel_test_parity.py" % fuchsia_dir
    )

    files_tmp = ctx.io.tempfile(
        "\n".join(files_to_pass) + "\n",
        name = "affected_files.txt",
    )
    output_tmp = ctx.io.tempfile("", name = "findings.json")

    cmd = [
        python_bin,
        "-B",
        checker_script,
        "--root",
        ctx.scm.root,
        "--files-from",
        files_tmp,
        "--output",
        output_tmp,
    ]
    if commits:
        msg_tmp = ctx.io.tempfile(commits[0].message, name = "commit_msg.txt")
        cmd.extend(["--commit-msg-file", msg_tmp])

    res = os_exec(
        ctx,
        cmd,
        env = {"PYTHONDONTWRITEBYTECODE": "1"},
        ok_retcodes = (0, 1),
    ).wait()

    raw_output = str(ctx.io.read_file(output_tmp)).strip()
    if not raw_output:
        if res.retcode != 0:
            fail("check_bazel_test_parity.py failed:\n%s" % res.stderr)
        return

    findings = json.decode(raw_output)
    shac_textproto_path = "%s/shac.textproto" % ctx.scm.root
    supports_label_prop = (
        bool(ctx.io.stat(shac_textproto_path)) and
        "fail_if_missing_label" in str(ctx.io.read_file(shac_textproto_path))
    )

    for finding in findings:
        filepath = finding.get("filepath", "")
        line = finding.get("line", 0)
        end_line = finding.get("end_line", 0)
        msg = finding["message"]
        level = finding["level"]
        props = finding.get("properties")

        # Shac rejects file-scoped findings on deleted files or paths outside
        # the live workspace; emit those as check-level findings instead.
        if filepath and filepath not in all_live_files:
            filepath = ""
            line = 0
            end_line = 0

        kwargs = {
            "level": level,
        }
        if filepath:
            kwargs["filepath"] = filepath
            if line > 0:
                kwargs["line"] = line
                if end_line >= line:
                    kwargs["end_line"] = end_line
        if props and supports_label_prop:
            kwargs["properties"] = props

        ctx.emit.finding(
            message = msg,
            **kwargs
        )

def register_bazel_test_parity_checks():
    """Registers the Bazel test parity shac check."""
    shac.register_check(
        shac.check(_bazel_test_parity, name = "bazel_test_parity", formatter = False),
    )
