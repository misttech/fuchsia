# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def _skill_format(ctx):
    """Validates and formats modified skills files."""
    exe_interp = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        get_fuchsia_dir(ctx),
        cipd_platform_name(ctx),
    )
    exe_script = "%s/scripts/skill_linter/skill_linter.py" % get_fuchsia_dir(ctx)

    skill_files = ctx.scm.affected_files(glob = ["**/SKILL.md"])

    if not skill_files:
        return

    file_paths = [ctx.scm.root + "/" + f for f in skill_files]
    res = os_exec(
        ctx,
        [exe_interp, exe_script, "--suggest-fix-in-json"] + file_paths,
        env = {
            "PYTHONPATH": get_fuchsia_dir(ctx) + "/third_party/pyyaml/src/lib",
        },
        ok_retcodes = [0, 1],
    ).wait()

    # Check for failure to produce JSON.
    if not res.stdout.strip().startswith("["):
        ctx.emit.finding(
            level = "error",
            message = "Skill linter error:\n%s" % res.stderr,
        )
        return

    findings = json.decode(res.stdout)
    for finding in findings:
        ctx.emit.finding(
            level = finding["level"],
            filepath = finding["filepath"],
            message = finding["message"],
            replacements = finding.get("replacements"),
        )

def register_skills_checks():
    shac.register_check(shac.check(_skill_format, formatter = True))
