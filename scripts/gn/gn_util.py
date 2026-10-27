# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import functools
import json
import subprocess


def gn_refs(target):
    """Invokes fx gn refs [target] returning the references as a list"""
    return (
        run_command(["fx", "gn", "refs", get_out_dir(), target])
        .strip()
        .splitlines()
    )


def run_command(command):
    """Runs command and returns stdout output"""
    return subprocess.check_output(
        command, stderr=subprocess.STDOUT, encoding="utf8"
    )


# functools.cache is more semantically accurate, but requires Python 3.9
@functools.lru_cache
def get_out_dir():
    """Retrieve the build output directory"""

    fx_status = run_command(["fx", "status", "--format=json"])
    return json.loads(fx_status)["environmentInfo"]["items"]["build_dir"][
        "value"
    ]
