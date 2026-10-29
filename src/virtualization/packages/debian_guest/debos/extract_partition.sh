#!/bin/bash
#
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Expected environment variables:
#
# "ARTIFACTDIR": the debos artifact directory.

set -e

# Create a qcow2 file from the partition.
#
# The raw disk image debos provides begins with a (GPT-based) partition table,
# consisting of a single partition. We only want the single partition, and not
# the leading/trailing GPT metadata.
#
# To get the partition contents, we umount the filesystem and just copy out
# the raw data from the partition's block device.
#

# The block device to operate on.
#
# The name "root" comes from the "image-partition" action's partition label.
BLOCK_DEVICE=/dev/disk/by-label/root

# Unmount the disk.
umount ${BLOCK_DEVICE}

# Zero out free blocks in the image. This zeros out temporary/deleted data,
# improving compression of the final image.
zerofree  ${BLOCK_DEVICE}

# Create an image from the partition.
#
# Write to a local path inside the VM first before moving to ARTIFACTDIR, as
# creating a qcow2 image directly on fakemachine's 9p mount can fail when
# qemu-img immediately re-reads the header.
LOCAL_QCOW2=$(mktemp --suffix=.qcow2)
qemu-img convert -f raw -O qcow2 ${BLOCK_DEVICE} "${LOCAL_QCOW2}"
mv --force "${LOCAL_QCOW2}" "${ARTIFACTDIR}/rootfs.qcow2"

# Remount the disk.
mount ${BLOCK_DEVICE} ${IMAGEMNTDIR}
