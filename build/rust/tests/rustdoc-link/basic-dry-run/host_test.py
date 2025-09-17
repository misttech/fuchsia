# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import unittest
from pathlib import Path
from sys import argv

assert (
    len(argv) > 0
), "host_test.py expects to be passed path to generated + linked docs"
_rustdoc_actions = Path(argv.pop())


class Test(unittest.TestCase):
    _rustdoc_actions: Path

    @classmethod
    def setUpClass(cls) -> None:
        cls._rustdoc_actions = _rustdoc_actions

    def test_dry_run_output(self) -> None:
        found_actions_string = self._rustdoc_actions.read_text()
        found_actions_json = json.loads(found_actions_string)

        # output should look like this:
        intended_output_string = """
{
    "host_action": {
        "build_action": null,
        "rustdoc_action": {
            "argfile": "docs/rust/argfiles/host.args"
        },
        "copy_action": {
            "srcs": [
                "host_x64/gen/build/rust/tests/rustdoc-link/basic-dry-run/bar.aux.doc/."
            ],
            "dst": "docs/rust/doc/host"
        }
    },
    "fuchsia_action": {
        "build_action": null,
        "rustdoc_action": {
            "argfile": "docs/rust/argfiles/fuchsia.args"
        },
        "copy_action": {
            "srcs": [
                "gen/build/rust/tests/rustdoc-link/basic-dry-run/foo.aux.doc/."
            ],
            "dst": "docs/rust/doc"
        }
    },
    "zip_action": null
}
"""

        intended_output_json = json.loads(intended_output_string)

        # assertEquals is fine here because all arrays above have length one,
        # and python checks objects for equality. We should be strict with
        # asserting exact equality here. This test helps ensure that changes
        # to rustdoc-link.py are reflected to the infra builder recipe.

        # With that being said, if you have to change the above, you should
        # make a corresponding change in infra!

        self.assertEquals(intended_output_json, found_actions_json)
