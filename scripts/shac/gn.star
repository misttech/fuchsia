# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks for GN files."""

load("./common.star", "FORMATTER_MSG", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def gn_no_print(ctx):
    """Warns if .gn or .gni files contain print() statements.

    Args:
        ctx: A ctx instance.
    """
    for path, meta in ctx.scm.affected_files(glob = ["*.gn", "*.gni"]).items():
        for num, line in meta.affected_lines():
            if line.strip().startswith("#"):
                continue

            # Match print( but ignore commented lines
            matches = ctx.re.allmatches(r"(print\s*\()", line)
            if matches:
                ctx.emit.finding(
                    message = "Avoid print() in GN files. It pollutes stdout and breaks automated tools (like gndoc). Consider using temporary prints and removing them before landing.",
                    level = "warning",
                    filepath = path,
                    line = num,
                    col = matches[0].offset + 1,
                )
                break  # Only one finding per file to reduce noise.

def _gn_format(ctx):
    """Runs gn format on .gn and .gni files.

    Args:
        ctx: A ctx instance.
    """
    affected_files = ctx.scm.affected_files(glob = ["*.gn", "*.gni"])
    if not affected_files:
        return

    gn = "%s/prebuilt/third_party/gn/%s/gn" % (get_fuchsia_dir(ctx), cipd_platform_name(ctx))

    result = os_exec(
        ctx,
        [gn, "format", "--dry-run"] + list(affected_files),
        ok_retcodes = [0, 1, 2],
    ).wait()

    lines = result.stdout.splitlines()
    files_to_format = []
    has_errors = False
    for line in lines:
        if not line.strip():
            continue
        if line in affected_files:
            files_to_format.append(line)
        else:
            has_errors = True

    for f in files_to_format:
        formatted_contents = os_exec(
            ctx,
            [gn, "format", "--stdin"],
            stdin = ctx.io.read_file(f),
        ).wait().stdout
        ctx.emit.finding(
            level = "error",
            message = FORMATTER_MSG,
            filepath = f,
            replacements = [formatted_contents],
        )

    # `gn format --dry-run` has three output cases:
    # 1. Prints nothing if the file has the correct formatting.
    # 2. Prints the name of the file if it needs formatting changes.
    # 3. Prints a raw parser error (e.g., 'ERROR at :1:1: ...') to stdout if it fails to parse,
    #    but crucially does NOT print the filename of the broken file.
    #
    # Because of Case 3, we cannot tell from the batch output which file is broken.
    # To identify the broken file(s), we manually track which files successfully parsed but
    # need formatting (Case 2, stored in `files_to_format`).
    #
    # If we detected any parser errors (indicated by `has_errors`), we fall back to running
    # `gn format --stdin` one-by-one on all remaining files (Case 1 and Case 3, excluding
    # `files_to_format`). This allows us to capture the exact parser error and associate it
    # with the correct filename.
    #
    # The optimization here is that if `has_errors` is False, we can completely skip this
    # fallback check for all the Case 1 files that are already correctly formatted.
    if has_errors:
        files_to_check_for_errors = [f for f in affected_files if f not in files_to_format]
        errors = []
        for f in files_to_check_for_errors:
            res = os_exec(
                ctx,
                [gn, "format", "--stdin"],
                stdin = ctx.io.read_file(f),
                ok_retcodes = [0, 1, 2],
            ).wait()
            if res.retcode == 1:
                errors.append("{}:\n  {}".format(f, res.stdout))
        if errors:
            fail("\n" + "\n\n".join(errors))

def register_gn_checks():
    shac.register_check(shac.check(_gn_format, formatter = True))
    shac.register_check(gn_no_print)
