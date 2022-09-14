#!/bin/sh
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

TOOL=$1
shift
STAMP=$1
shift

if [[ "$STAMP" != "--" ]]; then
  touch $STAMP
fi

export PYTHONPATH=`dirname $TOOL`/../lib/python3.8/site-packages
OUTPUT=`python3.8 $TOOL $@ 2>&1`

if [[ "$OUTPUT" != "" ]]; then
  echo "'$TOOL $@' failed:"
  echo "$OUTPUT"
  exit 1
fi


