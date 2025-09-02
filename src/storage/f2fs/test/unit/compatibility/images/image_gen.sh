#!/bin/bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

IMAGE_SIZE=100M
MOUNT_DIR=image_mnt

setup() {
  local image_name=${1}
  local is_inline=${2:-false}

  truncate --size ${IMAGE_SIZE} ${image_name}
  mkfs.f2fs -f ${image_name}
  if ${is_inline}; then
    mount -t f2fs -o inline_data,inline_dentry ${image_name} ${MOUNT_DIR}
  else
    mount -t f2fs ${image_name} ${MOUNT_DIR}
  fi
  pushd ${MOUNT_DIR}
}

clean() {
  local image_name=$1

  popd
  umount ${MOUNT_DIR}
  zstd ${image_name}
  rm ${image_name}
}

simple_io_test() {
  IMAGE="simple_io.img"
  setup ${IMAGE}

  echo "hello" > test

  clean ${IMAGE}
}

directory_test() {
  IMAGE="directory_test.img"
  setup ${IMAGE}

  mkdir -p depth
  current="depth"
  for i in $(seq 0 59); do
    current="$current/$i"
    mkdir -p "$current"
  done

  mkdir -p width
  for i in $(seq 0 59); do
    mkdir -p "width/$i"
  done

  for i in $(seq 20 39); do
    mv "width/$i" "width/$((i + 100))"
  done

  for i in $(seq 40 59); do
    rm -rf "width/$i"
  done

  clean ${IMAGE}
}

mkdir -p ${MOUNT_DIR}
simple_io_test
directory_test
rmdir ${MOUNT_DIR}
