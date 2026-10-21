# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Check for correctly formatted Fuchsia bug URLs."""

def bug_urls(ctx):
    """Checks that fuchsia bug URLs are correctly formatted.

    Bug URLs should use the form "https://fxbug.dev/<id>"; the form
    "http://fxb/<id>" isn't usable by non-Google employees, and
    "fxbug.dev/<id>" doesn't automatically linkify in most editors.

    Args:
        ctx: A ctx instance.
    """
    correct_format = "https://fxbug.dev/"
    for f, meta in ctx.scm.affected_files().items():
        for num, line in meta.affected_lines():
            for match in ctx.re.allmatches(
                r"(https?://)?fxb(ug\.dev)?/(\d+)",
                line,
            ):
                if match.groups[0].startswith(correct_format):
                    continue
                bug_number = match.groups[-1]
                repl = correct_format + bug_number
                end_offset = match.offset + len(match.groups[0])

                # Ignore invalid shortlinks if they're wrapped in square
                # brackets, which likely indicates markdown formatting where the
                # text is a link title rather than the link itself.
                if (match.offset > 0 and line[match.offset - 1] == "[") and (
                    end_offset < len(line) and line[end_offset] == "]"
                ):
                    continue

                ctx.emit.finding(
                    level = "warning",
                    message = "Bug links should use the form %s." % repl,
                    filepath = f,
                    line = num,
                    col = match.offset + 1,
                    end_col = end_offset + 1,
                    replacements = [repl],
                )
