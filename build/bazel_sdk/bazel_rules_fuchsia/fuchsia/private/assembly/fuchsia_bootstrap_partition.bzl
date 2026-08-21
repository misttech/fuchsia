# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for creating a bootstrap partition mapping."""

load(":providers.bzl", "FuchsiaPartitionInfo")

def _fuchsia_bootstrap_partition_impl(ctx):
    bootstrap_partition = {
        "name": ctx.attr.partition_name,
        "image": ctx.file.image.path,
    }
    if ctx.attr.condition_variable != "":
        bootstrap_partition["condition"] = {
            "variable": ctx.attr.condition_variable,
            "value": ctx.attr.condition_value,
        }
    if ctx.attr.condition_json != "":
        # Validate that condition_json contains valid JSON
        json.decode(ctx.attr.condition_json)
        bootstrap_partition["condition_json"] = ctx.attr.condition_json

    return [
        DefaultInfo(files = depset(direct = [ctx.file.image])),
        FuchsiaPartitionInfo(
            partition = bootstrap_partition,
        ),
    ]

fuchsia_bootstrap_partition = rule(
    doc = """Define a partition mapping from partition to image.""",
    implementation = _fuchsia_bootstrap_partition_impl,
    provides = [FuchsiaPartitionInfo],
    attrs = {
        "partition_name": attr.string(
            doc = "Name of the partition.",
            mandatory = True,
        ),
        "image": attr.label(
            doc = "The bootstrap image file.",
            allow_single_file = True,
            mandatory = True,
        ),
        "condition_variable": attr.string(
            doc = "Variable part of the condition that needs to be met before flash.",
        ),
        "condition_value": attr.string(
            doc = "Value part of the condition that needs to be met before flash.",
        ),
        "condition_json": attr.string(
            doc = """JSON condition string for conditional flashing.
Typically constructed via `json.encode({...})`.

The JSON condition tree supports:
- Logical operators:
  - `{"and": [condition1, condition2, ...]}`: True if all sub-conditions
    evaluate to true.
  - `{"or": [condition1, condition2, ...]}`: True if any sub-condition
    evaluates to true.
  - `{"not": condition}`: True if the sub-condition evaluates to false.
- Base command condition:
  - `cmd` (string, required): Fastboot command to execute (`"getvar <var>"`
    or `"oem <cmd>"`).
  - `match_result_regex` (string, optional, defaults to `"OKAY.*"`): Regex
    matched against the final response (`"OKAY<msg>"` or `"FAIL<msg>"`).
    Automatically anchored to match the full response string (wrapped with
    `^(?:...)$`). Partial/prefix matching can be done using `.*` (e.g.
    `"OKAYsbdp-.*"`).
  - `match_info` (string or object, optional): Condition evaluated against
    the multiline concatenation (joined with `\\n`) of all `INFO` lines
    returned by the command. Can be a regex string or a nested
    `{"and": [...]}`, `{"or": [...]}`, or `{"not": ...}` condition.

Example:
```starlark
condition_json = json.encode({
    "and": [
        {
            "cmd": "getvar dpm",
            "match_result_regex": "OKAYsbdp-.*",
        },
        {
            "cmd": "oem dpm dump-sbdp",
            "match_result_regex": "OKAY",
            "match_info": {
                "and": [
                    "gdmc = 1",
                    "cpm = 1",
                ],
            },
        },
    ],
})
```""",
        ),
    },
)
