# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from merge_extension import merge_extension_field, remove_field_from_extensions


class TestMergeExtension(unittest.TestCase):
    def test_remove_field_from_extensions(self) -> None:
        cases = [
            ("  extensions 76;\n", "76", "  \n"),
            (
                "  extensions 76, 100 to 200;\n",
                "76",
                "  extensions 100 to 200;\n",
            ),
            ("  extensions 10 to 20, 76;\n", "76", "  extensions 10 to 20;\n"),
            (
                "  extensions 10 to 20, 76, 100 to 200;\n",
                "76",
                "  extensions 10 to 20, 100 to 200;\n",
            ),
            (
                "  extensions\n    76,\n    100 to 200;\n",
                "76",
                "  extensions\n    100 to 200;\n",
            ),
            (
                "  extensions\n    10 to 20,\n    76;\n",
                "76",
                "  extensions\n    10 to 20;\n",
            ),
            (
                "  extensions\n    10 to 20,\n    76,\n    100 to 200;\n",
                "76",
                "  extensions\n    10 to 20,\n    100 to 200;\n",
            ),
        ]
        for input_content, field_id, expected in cases:
            actual = remove_field_from_extensions(input_content, field_id)
            self.assertEqual(actual, expected)

    def test_merge_extension_field(self) -> None:
        input_proto = (
            "message TracePacket {\n"
            "  oneof data {\n"
            "    string test = 1;\n"
            "  }\n"
            "}\n"
        )
        expected = (
            "message TracePacket {\n"
            "  oneof data {\n"
            "    string test = 1;\n"
            "    FrameTimelineEvent frame_timeline_event = 76;\n"
            "  }\n"
            "}\n"
        )
        actual = merge_extension_field(
            input_proto, "FrameTimelineEvent", "frame_timeline_event", "76"
        )
        self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
