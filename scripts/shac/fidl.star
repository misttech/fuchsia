# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("./common.star", "FORMATTER_MSG", "compiled_tool_path", "os_exec")

def _fidl_format(ctx):
    """Runs fidl-format.

    Args:
      ctx: A ctx instance.
    """
    exe = compiled_tool_path(ctx, "fidl-format")

    fidl_files = [
        f
        for f in ctx.scm.affected_files(glob = [
            "*.fidl",
            "!*.noformat.test.fidl",
        ])
        # Make sure the file itself ends with ".fidl" to exclude files in
        # directories that end with ".fidl".
        if f.endswith(".fidl")
    ]

    procs = []
    for f in fidl_files:
        procs.append((f, os_exec(ctx, [exe, f])))
    for f, proc in procs:
        formatted = proc.wait().stdout
        original = str(ctx.io.read_file(f))
        if formatted != original:
            ctx.emit.finding(
                level = "error",
                message = FORMATTER_MSG,
                filepath = f,
                replacements = [formatted],
            )

def _gidl_format(ctx):
    """Runs gidl-format.

    Args:
      ctx: A ctx instance.
    """
    exe = compiled_tool_path(ctx, "gidl-format")

    procs = [
        (f, os_exec(ctx, [exe, f]))
        for f in ctx.scm.affected_files(glob = "*.gidl")
    ]
    for f, proc in procs:
        formatted = proc.wait().stdout
        original = str(ctx.io.read_file(f))
        if formatted != original:
            ctx.emit.finding(
                level = "error",
                message = FORMATTER_MSG,
                filepath = f,
                replacements = [formatted],
            )

def _fidl_comment_check(ctx):
    """Warns about // comments in .fidl files preceding declarations."""
    for path, meta in ctx.scm.affected_files().items():
        if not path.endswith(".fidl") or path.endswith(".test.fidl"):
            continue

        content = str(ctx.io.read_file(path))
        lines = content.split("\n")

        for num, line in meta.new_lines():
            idx = num - 1

            if not ctx.re.allmatches(r"^\s*//($|[^/])", line):
                continue

            # Check exclusions
            if (ctx.re.allmatches(r"^\s*//\s*Copyright", line) or
                ctx.re.allmatches(r"^\s*//\s*Use of this source code", line) or
                ctx.re.allmatches(r"^\s*//\s*found in the LICENSE", line) or
                ctx.re.allmatches(r"^\s*//\s*TODO", line)):
                continue

            # Find the end of this comment block in the full file
            next_idx = idx + 1
            found_end = False
            for j in range(idx + 1, len(lines)):
                if not ctx.re.allmatches(r"^\s*//($|[^/])", lines[j]):
                    next_idx = j
                    found_end = True
                    break
            if not found_end:
                next_idx = len(lines)

            if next_idx >= len(lines):
                continue

            next_line = lines[next_idx]
            if next_line.strip() == "":
                # Blank line after comment block, allow it!
                continue

            # Check if it starts with an alphanumeric character or @
            if ctx.re.allmatches(r"^\s*[a-zA-Z0-9@]", next_line):
                ctx.emit.finding(
                    message = "Use /// instead of // for doc comments in FIDL files preceding declarations or members. fidldoc ignores // comments.",
                    level = "warning",
                    filepath = path,
                    line = num,
                )

def register_fidl_checks():
    shac.register_check(shac.check(_gidl_format, formatter = True))
    shac.register_check(shac.check(_fidl_format, formatter = True))
    shac.register_check(shac.check(_fidl_comment_check))
