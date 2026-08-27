# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""SHAC check for repository license compliance."""

load("./common.star", "compiled_tool_path", "os_exec")

def _license_project(ctx):
    """Runs `check-licenses validate` to verify repository license compliance.

    Args:
      ctx: A ctx instance.
    """
    exe = compiled_tool_path(ctx, "check-licenses")
    findings_file = ctx.io.tempfile("")

    res = os_exec(
        ctx,
        [exe, "validate", "-findings_file", findings_file],
        ok_retcodes = (0, 1),
    ).wait()

    raw_findings = str(ctx.io.read_file(findings_file)).strip()
    if raw_findings:
        findings = json.decode(raw_findings) or []
        for f in findings:
            ctx.emit.finding(
                level = f.get("level", "error"),
                message = f.get("message", ""),
                filepath = f.get("filepath"),
                line = f.get("line"),
                end_line = f.get("end_line"),
            )
    elif res.retcode != 0:
        message = res.stderr.strip()
        if not message:
            message = res.stdout.strip()
        ctx.emit.finding(
            level = "error",
            message = "License compliance check failed:\n{}".format(message),
        )

def register_license_project_checks():
    shac.register_check(shac.check(_license_project))
