# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC checks for Python files."""

load("./common.star", "FORMATTER_MSG", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def _python_format(ctx):
    """Formats Python code using autoflake, isort and black on a Python code base.

    Args:
      ctx: A ctx instance.
    """
    py_files = ctx.scm.affected_files(glob = ["*.py", "!third_party", "!*_pb2.py"])
    if not py_files:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        platform,
    )
    output = ctx.io.tempfile("")
    res = os_exec(
        ctx,
        [
            python_bin,
            "scripts/shac/python_format.py",
            "--python",
            python_bin,
            "--autoflake",
            "%s/third_party/pylibs/autoflake/main.py" % fuchsia_dir,
            "--isort",
            "%s/third_party/pylibs/isort/main.py" % fuchsia_dir,
            "--black",
            "%s/prebuilt/third_party/black/%s/black" % (fuchsia_dir, platform),
            "--pyproject-toml",
            "%s/pyproject.toml" % fuchsia_dir,
            # The output can exceed the limit on a subprocess's stdout size.
            "--output",
            output,
        ] + list(py_files),
        raise_on_failure = False,
    ).wait()
    if res.retcode != 0:
        # Don't let shac print the command, which includes every file path.
        fail("python_format.py failed:\n%s" % res.stderr)
    for filepath, formatted in json.decode(str(ctx.io.read_file(output))).items():
        ctx.emit.finding(
            level = "error",
            message = FORMATTER_MSG,
            filepath = filepath,
            replacements = [formatted],
        )

def _py_shebangs(ctx):
    """Validates that all Python script shebangs specify the vendored Python interpeter.

    Scripts can opt out of this by adding a comment with
    "allow-non-vendored-python" in a line after the shebang.
    """
    ignore_paths = (
        "build/bazel/",
        "build/bazel_sdk/",
        "infra/",
        "integration/",
        "vendor/",
        "third_party/",
    )
    for path in ctx.scm.affected_files(glob = "*.py"):
        if path.startswith(ignore_paths):
            continue
        lines = str(ctx.io.read_file(path, 4096)).splitlines()
        if not lines:
            continue
        first_line = lines[0]
        want_shebang = "#!/usr/bin/env fuchsia-vendored-python"
        if first_line.startswith("#!") and first_line != want_shebang:
            if len(lines) > 1 and lines[1].startswith("# allow-non-vendored-python"):
                continue
            ctx.emit.finding(
                level = "warning",
                message = "Use fuchsia-vendored-python in shebangs for Python scripts.",
                filepath = path,
                line = 1,
                replacements = [want_shebang + "\n"],
            )

def register_python_checks():
    shac.register_check(shac.check(_python_format, formatter = True))
    shac.register_check(_py_shebangs)
