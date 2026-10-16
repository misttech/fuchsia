# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines SHAC checks for C and C++ files."""

load(
    "./common.star",
    "FORMATTER_MSG",
    "cipd_platform_name",
    "get_build_dir",
    "get_fuchsia_dir",
    "os_exec",
)

# Paths excluded from C/C++ formatting and static analysis checks.
_IGNORED_GLOBS = [
    "!/build/bazel/fuchsia_idk/validation_data/**",
    "!/build/sdk/generate_prebuild_idk/validation_data/**",
    "!/src/devices/tools/fidlgen_banjo/tests/**",
    "!**/goldens/**",
    "!**/third_party/**",
]

# Maximum number of affected C/C++ files to analyze with clang-tidy in a single run.
_MAX_CLANG_TIDY_FILES = 100

def _clang_format(ctx):
    """Formats C/C++/Proto code using clang-format."""
    cpp_files = list(ctx.scm.affected_files(glob = [
        "*.c",
        "*.cc",
        "*.cpp",
        "*.h",
        "*.hh",
        "*.hpp",
        "*.proto",
    ] + _IGNORED_GLOBS).keys())
    if not cpp_files:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    clang_format_bin = "%s/prebuilt/third_party/clang/%s/bin/clang-format" % (
        fuchsia_dir,
        platform,
    )

    base_cmd = [
        clang_format_bin,
        "-style=file",
        "-fallback-style=Google",
        "-sort-includes",
    ]

    batch_size = 500
    dry_run_procs = []
    for i in range(0, len(cpp_files), batch_size):
        batch = cpp_files[i:i + batch_size]
        dry_run_procs.append(
            os_exec(
                ctx,
                base_cmd + ["--dry-run", "--Werror", "--ferror-limit=1"] + batch,
                ok_retcodes = [0, 1],
            ),
        )

    violation_suffix = ": error: code should be clang-formatted [-Wclang-format-violations]"
    unformatted = []
    for proc in dry_run_procs:
        res = proc.wait()
        batch_unformatted = []
        for line in res.stderr.splitlines():
            if line.endswith(violation_suffix):
                filepath = line[:-len(violation_suffix)].rsplit(":", 2)[0]
                batch_unformatted.append(filepath)
        if res.retcode and not batch_unformatted:
            fail("clang-format failed:\n%s" % res.stderr)
        unformatted.extend(batch_unformatted)

    procs = []
    for filepath in unformatted:
        procs.append((
            filepath,
            os_exec(ctx, base_cmd + [filepath]),
        ))

    for filepath, proc in procs:
        formatted = proc.wait().stdout

        # Protobuf string fields in SARIF output (--json-output) require valid UTF-8.
        # If a file contains non-UTF-8 bytes (e.g. Latin-1/Windows-1252 comments),
        # omit replacements so shac does not crash marshaling sarif.ArtifactContent.text.
        is_valid_utf8 = (formatted == "".join(formatted.codepoints()))
        if is_valid_utf8:
            msg = FORMATTER_MSG
            replacements = [formatted]
        else:
            msg = (
                "File not formatted and contains non-UTF-8 bytes. " +
                "Convert the file to UTF-8 or add it to _IGNORED_GLOBS " +
                "if it should not be formatted."
            )
            replacements = []
        ctx.emit.finding(
            # Switch to "error" once existing unformatted files in the tree are cleaned up.
            level = "warning",
            message = msg,
            filepath = filepath,
            replacements = replacements,
        )

def _header_guards(ctx):
    """Checks and formats C/C++ header guards."""
    headers = list(ctx.scm.affected_files(glob = [
        "*.h",
    ] + _IGNORED_GLOBS).keys())
    if not headers:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        platform,
    )
    checker_script = "%s/scripts/shac/check_header_guards.py" % fuchsia_dir

    # The results can exceed shac's limit on a subprocess's stdout size.
    output = ctx.io.tempfile("")
    os_exec(
        ctx,
        [
            python_bin,
            checker_script,
            "--root",
            fuchsia_dir,
            "--json",
            "--output",
            output,
        ] + headers,
    ).wait()

    for item in json.decode(str(ctx.io.read_file(output))):
        if "error" in item:
            ctx.emit.finding(
                level = "warning",
                message = item["error"] or ("Header guard issue in %s." % item["path"]),
                filepath = item["path"],
            )
        else:
            ctx.emit.finding(
                level = "warning",
                message = FORMATTER_MSG,
                filepath = item["path"],
                replacements = item["replacements"],
            )

def _clang_tidy(ctx):
    """Runs clang-tidy on C/C++ source files."""
    cpp_affected = ctx.scm.affected_files(glob = [
        "*.c",
        "*.cc",
        "*.cpp",
    ] + _IGNORED_GLOBS)

    header_affected = ctx.scm.affected_files(glob = [
        "*.h",
        "*.hh",
        "*.hpp",
    ] + _IGNORED_GLOBS)

    # SHAC's `ctx.scm.affected_files()` populates `meta.action` differently
    # depending on how SHAC is invoked:
    # 1. `shac check` (default git diff) or `shac check --all`: SHAC uses its
    #    `gitCheckout` backend, setting `meta.action` to the git diff-filter
    #    code (e.g. "M", "A") for modified/added files and `""` for untouched
    #    files. Filtering to non-empty `meta.action` narrows `--all` runs down
    #    to only the files actually modified in git.
    # 2. `shac check <file1> ...` (e.g. `fx lint --files=...`): SHAC uses its
    #    `specifiedFiles` backend, which does not query git and sets
    #    `meta.action == ""` on all passed files (even if modified in git).
    #
    # Therefore, if any file has a non-empty `meta.action`, analyze only those
    # modified/added files; otherwise fall back to all returned keys (which handles
    # explicit file arguments, while `_MAX_CLANG_TIDY_FILES` below still skips
    # `--all` runs when zero C/C++ files were modified in git).
    cpp_modified = [f for f, m in cpp_affected.items() if m.action]
    header_modified = [f for f, m in header_affected.items() if m.action]
    if cpp_modified or header_modified:
        cpp_files = cpp_modified
        header_files = header_modified
    else:
        cpp_files = list(cpp_affected.keys())
        header_files = list(header_affected.keys())

    if not cpp_files and not header_files:
        return

    # Full AST compilation is computationally expensive (~1-5 seconds per file).
    # Skip analysis when the number of target C/C++ files exceeds the threshold
    # (such as on large C++ refactors or `--all` runs with no modified C/C++ files).
    total_files = len(cpp_files) + len(header_files)
    if total_files > _MAX_CLANG_TIDY_FILES:
        return

    # clang-tidy requires the build output directory to locate compile_commands.json
    # and generated headers (such as FIDL bindings). If fuchsia_build_dir is not
    # configured, we cannot resolve compilation flags and must exit early.
    build_dir_var = ctx.vars.get("fuchsia_build_dir")
    if not build_dir_var:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        platform,
    )
    clang_tidy_bin = "%s/prebuilt/third_party/clang/%s/bin/clang-tidy" % (
        fuchsia_dir,
        platform,
    )
    driver_script = "%s/scripts/shac/clang_tidy.py" % fuchsia_dir
    build_dir = get_build_dir(ctx)

    res = os_exec(
        ctx,
        [
            python_bin,
            driver_script,
            "--clang-tidy",
            clang_tidy_bin,
            "--build-dir",
            build_dir,
            "--root",
            ctx.scm.root,
        ] + cpp_files + header_files,
    ).wait()

    if res.stderr:
        print(res.stderr.strip())  # allow-print

    for finding in json.decode(res.stdout):
        if "line" in finding:
            ctx.emit.finding(
                level = "warning",
                message = finding["message"],
                filepath = finding["filepath"],
                line = finding["line"],
                col = finding["col"],
            )
        else:
            ctx.emit.finding(
                level = "warning",
                message = finding["message"],
                filepath = finding["filepath"],
            )

def register_cpp_checks():
    shac.register_check(shac.check(_clang_format, formatter = True))
    shac.register_check(shac.check(_clang_tidy))
    shac.register_check(shac.check(_header_guards, formatter = True))
