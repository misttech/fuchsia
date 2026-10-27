#!/usr/bin/env fuchsia-vendored-python
#
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import json
import unittest
from collections.abc import Mapping, Sequence
from typing import Any
from unittest import mock

import treemap


def _base_kernel_and_root_job(
    physmem_bytes: int = 100 * 1024, vmo_bytes: int = 64 * 1024
) -> Sequence[Mapping[str, Any]]:
    return [
        {
            "id": "kernel/physmem",
            "type": "kernel",
            "parent": "",
            "name": "physmem",
            "size_bytes": physmem_bytes,
        },
        {
            "id": "kernel/free",
            "type": "kernel",
            "parent": "kernel/physmem",
            "name": "free",
            "size_bytes": physmem_bytes - vmo_bytes,
        },
        {
            "id": "kernel/vmo",
            "type": "kernel",
            "parent": "kernel/physmem",
            "name": "vmo",
            "size_bytes": vmo_bytes,
        },
        {
            "id": "j/1000",
            "type": "j",
            "koid": 1000,
            "parent": "kernel/vmo",
            "name": "root",
        },
    ]


class TreemapTests(unittest.TestCase):
    def test_format_size(self) -> None:
        self.assertEqual(treemap.format_size(0), "0B")
        self.assertEqual(treemap.format_size(512), "512B")
        self.assertEqual(treemap.format_size(1024), "1k")
        self.assertEqual(treemap.format_size(17 * 1024), "17k")
        self.assertEqual(treemap.format_size(1024 * 1024), "1M")
        self.assertEqual(treemap.format_size(10999), "10.8k")
        self.assertEqual(treemap.format_size(11220), "11.0k")

    def test_iob_handle(self) -> None:
        # VMO 500 is an unmapped IOBuffer shared by two processes via
        # "IOB_HANDLE". VMO 501 is mapped in p/2001 and also held via handle in
        # p/2002.
        dataset = [
            *_base_kernel_and_root_job(
                physmem_bytes=64 * 1024, vmo_bytes=32 * 1024
            ),
            {
                "id": "p/2001",
                "type": "p",
                "koid": 2001,
                "parent": "j/1000",
                "name": "archivist.cm",
                "private_bytes": 8192,
                "shared_bytes": 0,
                "pss_bytes": 8192,
                "vmos": [
                    {
                        "koid": 500,
                        "name": "iob-ring",
                        "size_bytes": 4096,
                        "num_mappings": 0,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                    {
                        "koid": 501,
                        "name": "mapped-ring",
                        "size_bytes": 8192,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 8192,
                        "committed_scaled_bytes": 8192,
                    },
                ],
                "vmo_refs": [
                    {"vmo_koid": 500, "via": ["IOB_HANDLE"]},
                    {"vmo_koid": 501, "via": ["IOB_HANDLE", "MAPPING"]},
                ],
            },
            {
                "id": "p/2002",
                "type": "p",
                "koid": 2002,
                "parent": "j/1000",
                "name": "client.cm",
                "private_bytes": 0,
                "shared_bytes": 0,
                "pss_bytes": 0,
                "vmos": [
                    {
                        "koid": 500,
                        "name": "iob-ring",
                        "size_bytes": 4096,
                        "num_mappings": 0,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                    {
                        "koid": 501,
                        "name": "mapped-ring",
                        "size_bytes": 8192,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 8192,
                        "committed_scaled_bytes": 8192,
                    },
                ],
                "vmo_refs": [
                    {"vmo_koid": 500, "via": ["IOB_HANDLE"]},
                    {"vmo_koid": 501, "via": ["IOB_HANDLE"]},
                ],
            },
            {"type": "__META", "timestamp": "2026-10-23T00:00:00.000Z"},
        ]

        root = treemap.build_tree(dataset)
        self.assertEqual(root.area, 64 * 1024)
        # p/2001 gets 2048 (half of unmapped VMO 500) + 8192 (mapped VMO 501).
        self.assertEqual(treemap.lookup("p/2001").area, 2048 + 8192)
        # p/2002 gets 2048 (half of unmapped VMO 500) + 0 (VMO 501 is already
        # attributed to mapping process p/2001).
        self.assertEqual(treemap.lookup("p/2002").area, 2048)
        self.assertEqual(treemap.lookup("j/1000").area, 4096 + 8192)
        self.assertEqual(
            treemap.lookup("kernel/vmo/unknown").area,
            32 * 1024 - (4096 + 8192),
        )

    def test_starnix_shared_aspace_and_cow_scaled_bytes(self) -> None:
        # Three Starnix processes sharing one VmAspace (kernel reports
        # share_count == 1) and referencing a COW clone whose committed_bytes
        # is 24 KiB but committed_scaled_bytes is 12 KiB.
        shared_vmo = {
            "koid": 700,
            "name": "starnix-anon",
            "size_bytes": 24 * 1024,
            "num_mappings": 1,
            "share_count": 1,
            "committed_bytes": 24 * 1024,
            "committed_scaled_bytes": 12 * 1024,
        }
        dataset = [
            *_base_kernel_and_root_job(
                physmem_bytes=64 * 1024, vmo_bytes=32 * 1024
            ),
            *(
                {
                    "id": f"p/{3000 + idx}",
                    "type": "p",
                    "koid": 3000 + idx,
                    "parent": "j/1000",
                    "name": f"starnix_proc_{idx}",
                    "private_bytes": 0,
                    "shared_bytes": 12 * 1024,
                    "pss_bytes": 4 * 1024,
                    "vmos": [shared_vmo],
                    "vmo_refs": [{"vmo_koid": 700, "via": ["MAPPING"]}],
                }
                for idx in range(3)
            ),
        ]

        treemap.build_tree(dataset)
        for idx in range(3):
            self.assertEqual(treemap.lookup(f"p/{3000 + idx}").area, 4 * 1024)
        self.assertEqual(treemap.lookup("j/1000").area, 12 * 1024)

    def test_zero_area_vmos_skipped_and_unnamed_aggregated(self) -> None:
        dataset = [
            *_base_kernel_and_root_job(
                physmem_bytes=64 * 1024, vmo_bytes=32 * 1024
            ),
            {
                "id": "p/4000",
                "type": "p",
                "koid": 4000,
                "parent": "j/1000",
                "name": "worker",
                "private_bytes": 8192,
                "shared_bytes": 0,
                "pss_bytes": 8192,
                "vmos": [
                    {
                        "koid": 801,
                        "name": "",
                        "size_bytes": 4096,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                    {
                        "koid": 802,
                        "name": "",
                        "size_bytes": 4096,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                    {
                        "koid": 803,
                        "name": "uncommitted",
                        "size_bytes": 65536,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 0,
                        "committed_scaled_bytes": 0,
                    },
                    {
                        "koid": 804,
                        "name": "restricted_state_vmo:9001",
                        "size_bytes": 4096,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                    {
                        "koid": 805,
                        "name": "restricted_state_vmo:9002",
                        "size_bytes": 4096,
                        "num_mappings": 1,
                        "share_count": 1,
                        "committed_bytes": 4096,
                        "committed_scaled_bytes": 4096,
                    },
                ],
                "vmo_refs": [
                    {"vmo_koid": 801, "via": ["MAPPING"]},
                    {"vmo_koid": 802, "via": ["MAPPING"]},
                    {"vmo_koid": 803, "via": ["MAPPING"]},
                    {"vmo_koid": 804, "via": ["MAPPING"]},
                    {"vmo_koid": 805, "via": ["MAPPING"]},
                ],
            },
        ]

        root = treemap.build_tree(dataset)
        proc = treemap.lookup("p/4000")
        self.assertEqual(len(proc.children), 2)
        labels = {child.html_label(): child.area for child in proc.children}
        self.assertDictEqual(
            labels,
            {
                '<span class="treemap-node-type treemap-node-type-v">V</span> <i>UNNAMED</i>[2]': 8192,
                '<span class="treemap-node-type treemap-node-type-v">V</span> restricted_state_vmo[2]': 8192,
            },
        )

        web = treemap.build_webtreemap(root)

        # Ensure no node in the webtreemap JSON has $area == 0.
        def check_positive_areas(entry: Mapping[str, Any]) -> None:
            self.assertGreater(entry["data"]["$area"], 0)
            for child in entry["children"]:
                check_positive_areas(child)

        check_positive_areas(web)

    def test_snapshot_skew_clamps_unknown_and_parent_areas(self) -> None:
        # Simulate snapshot skew where process VMOs exceed initial kernel/vmo.
        dataset = [
            *_base_kernel_and_root_job(
                physmem_bytes=16 * 1024, vmo_bytes=8 * 1024
            ),
            {
                "id": "p/5000",
                "type": "p",
                "koid": 5000,
                "parent": "j/1000",
                "name": "growing-proc",
                "private_bytes": 20 * 1024,
                "shared_bytes": 0,
                "pss_bytes": 20 * 1024,
            },
        ]

        root = treemap.build_tree(dataset)
        self.assertEqual(treemap.lookup("kernel/vmo/unknown").area, 0)
        self.assertEqual(treemap.lookup("kernel/vmo").area, 20 * 1024)
        self.assertEqual(root.area, 28 * 1024)

        table_html = "\n".join(treemap.dump_html_table(root))
        self.assertNotIn('value="-', table_html)

    @mock.patch.object(treemap, "sys", autospec=True, spec_set=True)
    def test_main_inlines_webtreemap_assets(
        self, mock_sys: mock.MagicMock
    ) -> None:
        dataset = _base_kernel_and_root_job(
            physmem_bytes=32 * 1024, vmo_bytes=16 * 1024
        )
        mock_sys.stdin.read.return_value = json.dumps(dataset)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            treemap.main()

        output = stdout.getvalue()
        self.assertIn("<!DOCTYPE html>", output)
        self.assertIn(".webtreemap-node {", output)
        self.assertIn("appendTreemap(map, kTree);", output)
        self.assertNotIn("<link rel='stylesheet'", output)
        self.assertNotIn("<script src=", output)

    @mock.patch.object(treemap, "os", autospec=True, spec_set=True)
    def test_read_webtreemap_asset_embedded_fallback(
        self, mock_os: mock.MagicMock
    ) -> None:
        # Verify the embedded .pyz importlib.resources fallback when run
        # outside a Fuchsia source checkout (such as on a Swarming bot).
        mock_os.path.isfile.return_value = False
        self.assertIn(
            ".webtreemap-node {",
            treemap._read_webtreemap_asset("webtreemap.css"),
        )
        self.assertIn(
            "function appendTreemap",
            treemap._read_webtreemap_asset("webtreemap.js"),
        )


if __name__ == "__main__":
    unittest.main()
