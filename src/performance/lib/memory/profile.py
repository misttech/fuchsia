# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Supports using `ffx profile memory` in e2e tests."""

import fnmatch
import json
from typing import Any, Mapping, cast

from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from reporting import metrics

_DESCRIPTION_BASE = "Total populated bytes for private uncompressed memory VMOs"


def capture(
    dut: FuchsiaDevice, principal_groups: Mapping[str, str] | None = None
) -> metrics.Report:
    """Captures kernel and user space memory metrics using `ffx profile memory`.
    See documentation at
    https://fuchsia.dev/fuchsia-src/development/tools/ffx/workflows/explore-memory-usage

    Args:
      dut: A FuchsiaDevice instance connected to the device to profile.
      principal_groups: mapping from group name to a `fnmatch` pattern
        that selects the principals by name. A metric labelled
        "Memory/Principal/{group_name}/PrivatePopulated" is returned for each
        item.

    Returns:
      metrics.Report containing two sets of memory measurements:
        - "Memory/Process/{starnix_kernel,binder}.PrivatePopulated": total populated
            bytes for VMOs. This is private uncompressed memory.
        - A whole-device memory digest, as retrieved by `ffx profile memory`.

    """
    principal_groups = principal_groups or {}

    component_profile = json.loads(
        dut.ffx.run(
            [
                "--machine",
                "json",
                "profile",
                "memory",
                "--backend",
                "memory_monitor_2",
            ],
        )
    )
    structured = process_component_profile(principal_groups, component_profile)
    description = metrics.describe_callable(capture)
    return metrics.Report(
        structured,
        {
            "memory_profile": _simplify_digest(component_profile),
        },
        [
            metrics.MetricsProcessorDescription(
                doc=description["doc"],
                code_path=description["code_path"],
                line_no=description["line_no"],
                metrics=[tcr.describe() for tcr in structured],
            )
        ],
    )


def process_component_profile(
    principal_groups: Mapping[str, str], component_profile: Any
) -> list[metrics.TestCaseResult]:
    results: list[metrics.TestCaseResult] = []
    for group_name, pattern in principal_groups.items():
        digest = component_profile["ComponentDigest"]
        private_populated = sum(
            principal["populated_private"]
            for principal in digest["principals"]
            if fnmatch.fnmatch(principal["name"], pattern)
        )
        results.append(
            metrics.TestCaseResult(
                label=f"Memory/Principal/{group_name}/PrivatePopulated",
                unit=metrics.Unit.bytes,
                values=[private_populated],
                doc=f"{_DESCRIPTION_BASE}: {group_name}",
            )
        )
    return results


def _simplify_name_to_vmo_memory(
    name_to_vmo_memory: metrics.JSON,
) -> list[metrics.JSON]:
    """Prepares `ffx profile memory` JSON data for BigQuery.

    Input sample:
        {
            "[blobs]": {
                "private": 0,
                "private_populated": 0,
                "scaled": 296959744,
                "scaled_populated": 296959744,
                "total": 871759872,
                "total_populated": 871759872,
                "vmos": [
                    153096,
                    127992
                ]
            }
        }

    Output sample:
        {
            "name": "[blobs]",
            "private": 0,
            "private_populated": 0,
            "scaled": 296959744,
            "scaled_populated": 296959744,
            "total": 871759872,
            "total_populated": 871759872,
        }
    """
    if not isinstance(name_to_vmo_memory, dict):
        raise ValueError
    return [
        dict(name=cast(metrics.JSON, k)) | _with_vmos_removed(v)
        for k, v in name_to_vmo_memory.items()
    ]


def _with_vmos_removed(metrics_dict: metrics.JSON) -> dict[str, metrics.JSON]:
    """Returns a copy of the specified directory without the "vmos" key."""
    if not isinstance(metrics_dict, dict):
        raise ValueError
    return {k: v for k, v in metrics_dict.items() if k != "vmos"}


def _simplify_principal(principal: metrics.JSON) -> dict[str, metrics.JSON]:
    """Prepares `ffx profile memory component` JSON data for BigQuery."""
    if not isinstance(principal, dict):
        raise ValueError
    return principal | {"vmos": _simplify_name_to_vmo_memory(principal["vmos"])}


def _simplify_principals(principals: metrics.JSON) -> list[metrics.JSON]:
    """Prepares `ffx profile memory` JSON data for BigQuery.

    Turns a list of principals into a list of simplified processes.
    """
    if not isinstance(principals, list):
        raise ValueError
    return [_simplify_principal(b) for b in principals]


def _simplify_digest(component_profile: metrics.JSON) -> metrics.JSON:
    result = {}

    if isinstance(component_profile, dict):
        digest = component_profile["ComponentDigest"]
        if not isinstance(digest, dict):
            raise ValueError
        result = digest | {
            "principals": _simplify_principals(digest["principals"])
        }

    return result
