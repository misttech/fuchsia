#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import subprocess
import sys

import python.runfiles.runfiles as runfiles

parser = argparse.ArgumentParser()
parser.add_argument("binary", help="Rlocation of python binary to invoke.")
args = parser.parse_args()

r = runfiles.Create()
test_location = r.Rlocation(args.binary)
test_env = r.EnvVars()

print(
    f"Found test at {test_location}\nEnvironment: {json.dumps(test_env, indent=2)}\ncwd = {os.getcwd()}\n"
)
subprocess.check_call([sys.executable, test_location], env=test_env)
print("Test run succesfully!")
