#!/usr/bin/env python3.8
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import re
import sys

import os.path

from typing import List, Optional

def check_allowed(allowlist: List[str], match: Optional[re.Match], root_build_dir: str):
    if not match:
        return True

    # Only allow "" includes.
    if match.group(1) != '"' or match.group(3) != '"':
        return False

    include_path = match.group(2)
    full_path = path.join(root_build_dir, include_path)
    return full_path in allowlist

def validate(args):
    with open(args.touch_file, 'w') as file:
        pass
    allowed = []
    with open(args.allowed_headers) as file:
        for line in file:
            allowed.append(line.strip())


    regex = re.compile(r'#include\s*("|<)([^">]+)("|>)')
    with open(args.dts_file) as file:
        for line in file:
            match = regex.search(line)
            if not check_allowed(allowed, match, args.root_build_dir):
                print("Bad include in {}: '{}'".format(args.dts_file, line.strip()), file=sys.stderr)
                sys.exit(1)









if __name__ == '__main__':
    parser = argparse.ArgumentParser('check_includes.py')
    parser.add_argument('allowed_headers', help='File containing list of allowed headers')
    parser.add_argument('dts_file', help='File to validate.')
    parser.add_argument('root_build_dir', help='Path to root build dir.')
    parser.add_argument('touch_file', help='Touch this file to make GN happy.')

    validate(parser.parse_args())
