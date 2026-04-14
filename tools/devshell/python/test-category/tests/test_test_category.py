# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Test for TESTING.json5 statistics.

As a special case, running this test will also print statistics computed from
the content of the testing_metadata.json5 file. It will also dump a freeform JSON file
(testing_categories.freeform.json) and a copy of the raw metadata to
FUCHSIA_TEST_OUTDIR if the latter is defined in the environment. See
//testing/testrunner/README.md for more details.
"""

import argparse
import json
import os
import shutil
import sys
import unittest

import main
import serialization

# Set by args. If unset we try to find the real version.
metadata_path_from_args: str


class TestTestCategory(unittest.IsolatedAsyncioTestCase):
    async def test_store_artifacts(self) -> None:
        metadata_path = metadata_path_from_args

        self.assertTrue(
            os.path.exists(metadata_path),
            f"Metadata not found at {metadata_path}.",
        )

        metadata = main.load_metadata_from_path(metadata_path)
        stats_dict = main.calculate_stats(metadata, [""], "")

        stats_json = json.dumps(
            serialization.instance_to_dict(stats_dict), indent=2
        )

        self.assertNotEqual(len(stats_dict.categories), 0)
        self.assertGreaterEqual(len(stats_dict.tags), 0)

        print("\nStats dict:\n{}\n".format(stats_json))

        # Check if output directory is specified
        outdir = os.environ.get("FUCHSIA_TEST_OUTDIR")
        if outdir:
            with open(
                os.path.join(outdir, "testing_categories.freeform.json"), "w"
            ) as f:
                f.write(stats_json)
            # Copy the raw JSON too
            shutil.copy(
                metadata_path, os.path.join(outdir, "testing_metadata.json")
            )

            print(f"Stored artifacts in {outdir}")
        else:
            print("FUCHSIA_TEST_OUTDIR not set. Not storing artifacts.")


def test_main() -> None:
    global metadata_path_from_args
    args = argparse.ArgumentParser()
    args.add_argument("--metadata-path", required=True)
    parsed_args, remaining_args = args.parse_known_args()
    metadata_path_from_args = parsed_args.metadata_path

    unittest.main(argv=sys.argv[0:1] + remaining_args)
