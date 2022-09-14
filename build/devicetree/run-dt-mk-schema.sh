#!/bin/sh
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

TOOL=$1
shift
DEPFILE=$1
shift
DEPFILE_SRC=${1:1}

# ninja appears to be very nice about depfiles.
echo "ninja_doesnt_need_anything_here:" > $DEPFILE
cat $DEPFILE_SRC  >> $DEPFILE

for file in `cat $DEPFILE_SRC`; do
  if [[ ! -e $file ]]; then
    echo "$0: No such file or directory: '$file'"
    exit 1
  fi
done

export PYTHONPATH=`dirname $TOOL`/../lib/python3.8/site-packages
OUTPUT=`python3.8 $TOOL $@ 2>&1`


if [[ "$OUTPUT" != "" ]]; then
  echo "'$TOOL $@' failed:"
  echo "$OUTPUT"
  exit 1
fi


