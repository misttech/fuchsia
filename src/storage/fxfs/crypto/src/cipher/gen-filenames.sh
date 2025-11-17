#!/bin/bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This script generates a Rust file with fscrypt metadata for 255 files
# created on a connected Android device (running Linux).

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
readonly DEVICE_DIR="/data/fscrypt_test_data_$$"
readonly PROTECTOR="fxfs_testing_protector_$$"
readonly RUST_FILE="${SCRIPT_DIR}/fscrypt_test_data.rs"
readonly FSCRYPTCTL_DIR="${SCRIPT_DIR}/fscryptctl"
readonly FSCRYPTCTL_BIN="${FSCRYPTCTL_DIR}/fscryptctl"
readonly HOST_TMP_DIR="$(mktemp -d)"
readonly IMAGE_NAME="f2fs.img"
readonly MOUNT_POINT="${DEVICE_DIR}/mnt"

if [[ ! -f "${FSCRYPTCTL_BIN}" ]]; then
  echo "fscryptctl not found. Downloading and building..."
  rm -rf "${FSCRYPTCTL_DIR}"
  git clone https://github.com/google/fscryptctl.git "${FSCRYPTCTL_DIR}"
  # Build statically.
  make -C "${FSCRYPTCTL_DIR}" fscryptctl LDFLAGS="-static"
fi

echo "Waiting for device..."
adb wait-for-device

cleanup() {
  echo "Cleaning up..."
  adb shell "umount ${MOUNT_POINT}" || true
  adb shell "rm -rf ${DEVICE_DIR}" || true
  rm -rf "${HOST_TMP_DIR}" || true
}
trap cleanup EXIT

echo "Setting up device..."
adb root
adb wait-for-device
adb shell "rm -rf ${DEVICE_DIR} && mkdir -p ${DEVICE_DIR}"
adb push "${FSCRYPTCTL_BIN}" /data/local/tmp/fscryptctl
adb shell "chmod +x /data/local/tmp/fscryptctl"

echo "Creating and formatting f2fs image..."
adb shell "dd if=/dev/zero of=${DEVICE_DIR}/${IMAGE_NAME} bs=1M count=100"
adb shell "make_f2fs -O encrypt,casefold -C utf8 ${DEVICE_DIR}/${IMAGE_NAME}"

echo "Mounting image..."
adb shell "mkdir -p ${MOUNT_POINT}"
adb shell "mount -t f2fs -o loop ${DEVICE_DIR}/${IMAGE_NAME} ${MOUNT_POINT}"

echo "Setting up encryption..."
adb shell "head -c 64 /dev/urandom > ${DEVICE_DIR}/key.bin"
# fscryptctl add_key prints the key identifier to stdout.
readonly KEY_ID=$(adb shell "/data/local/tmp/fscryptctl add_key ${MOUNT_POINT} < ${DEVICE_DIR}/key.bin")
echo "Key ID: ${KEY_ID}"

# Standard encrypted directory
adb shell "mkdir ${MOUNT_POINT}/encrypted_dir"
adb shell "/data/local/tmp/fscryptctl set_policy ${KEY_ID} ${MOUNT_POINT}/encrypted_dir --padding=16 --iv-ino-lblk-32"

# Casefold encrypted directory
adb shell "mkdir ${MOUNT_POINT}/casefold_encrypted_dir"
adb shell "chattr +F ${MOUNT_POINT}/casefold_encrypted_dir"
adb shell "/data/local/tmp/fscryptctl set_policy ${KEY_ID} ${MOUNT_POINT}/casefold_encrypted_dir --padding=16 --iv-ino-lblk-32"

echo "Creating files..."
adb shell "
set -e
cd ${MOUNT_POINT}/encrypted_dir
for i in \$(seq 1 255); do
  name=\$(printf \"%0.sA\" \$(seq 1 \$i))
  touch \"\$name\"
done

cd ${MOUNT_POINT}/casefold_encrypted_dir
for i in \$(seq 1 255); do
  name=\$(printf \"%0.sA\" \$(seq 1 \$i))
  touch \"\$name\"
done
"

echo "Getting unencrypted filenames..."
adb shell "ls -i1 ${MOUNT_POINT}/encrypted_dir" > "${HOST_TMP_DIR}/unencrypted.txt"
adb shell "ls -i1 ${MOUNT_POINT}/casefold_encrypted_dir" > "${HOST_TMP_DIR}/casefold_unencrypted.txt"

echo "Removing key..."
adb shell "/data/local/tmp/fscryptctl remove_key ${KEY_ID} ${MOUNT_POINT}"

echo "Getting encrypted filenames..."
adb shell "ls -i1 ${MOUNT_POINT}/encrypted_dir" > "${HOST_TMP_DIR}/encrypted.txt"
adb shell "ls -i1 ${MOUNT_POINT}/casefold_encrypted_dir" > "${HOST_TMP_DIR}/casefold_encrypted.txt"

echo "Processing file data..."
readonly FILE_DATA="$(python3 -c '
import sys

def read_list(filename):
    data = {}
    with open(filename, "r") as f:
        for line in f:
            parts = line.strip().split()
            if len(parts) >= 2:
                data[parts[0]] = parts[1]
    return data

unencrypted = read_list(sys.argv[1])
encrypted = read_list(sys.argv[2])

for inode, name in unencrypted.items():
    if inode in encrypted:
        print(f"{inode},{name},{encrypted[inode]}")
' "${HOST_TMP_DIR}/unencrypted.txt" "${HOST_TMP_DIR}/encrypted.txt")"

readonly CASEFOLD_FILE_DATA="$(python3 -c '
import sys

def read_list(filename):
    data = {}
    with open(filename, "r") as f:
        for line in f:
            parts = line.strip().split()
            if len(parts) >= 2:
                data[parts[0]] = parts[1]
    return data

unencrypted = read_list(sys.argv[1])
encrypted = read_list(sys.argv[2])

for inode, name in unencrypted.items():
    if inode in encrypted:
        print(f"{inode},{name},{encrypted[inode]}")
' "${HOST_TMP_DIR}/casefold_unencrypted.txt" "${HOST_TMP_DIR}/casefold_encrypted.txt")"

echo "Dumping encryption key..."
readonly KEY_BYTES="$(adb shell "cat ${DEVICE_DIR}/key.bin" | xxd -i)"

echo "Getting filesystem UUID..."
# Get the loop device associated with the mount
LOOP_DEV=$(adb shell "mount | grep ' ${MOUNT_POINT} ' | cut -d' ' -f1")
# Try blkid on the loop device
UUID_STR=$(adb shell "blkid ${LOOP_DEV} | sed -n 's/.*UUID=\"\([^\"]*\)\".*/\1/p'")

if [[ -n "${UUID_STR}" ]]; then
  # Convert UUID string to bytes for Rust array
  # Remove hyphens
  UUID_HEX=$(echo "${UUID_STR}" | sed 's/-//g')
  # Convert to Rust array format
  readonly UUID_BYTES=$(echo "${UUID_HEX}" | xxd -r -p | xxd -i)
else
  echo "Failed to get UUID from ${LOOP_DEV}"
  exit 1
fi

echo "Getting directory inodes..."
readonly DIR_INODE=$(adb shell "ls -id ${MOUNT_POINT}/encrypted_dir" | cut -d' ' -f1)
readonly CASEFOLD_DIR_INODE=$(adb shell "ls -id ${MOUNT_POINT}/casefold_encrypted_dir" | cut -d' ' -f1)

echo "Getting directory nonces..."
adb shell "umount ${MOUNT_POINT}"

get_nonce() {
  local inode=$1
  local device=$2
  local output=$(adb shell "dump.f2fs -i ${inode} ${device}")
  echo "$output" | python3 -c '
import sys
import re

content = sys.stdin.read()
match = re.search(r"nonce: ([0-9a-fA-F]{32})", content, re.IGNORECASE)
if match:
    nonce = match.group(1)
    print(", ".join([f"0x{nonce[i:i+2]}" for i in range(0, len(nonce), 2)]))
else:
    sys.stderr.write("Could not find nonce in dump.f2fs output\n")
    sys.exit(1)
'
}

readonly DIR_NONCE=$(get_nonce ${DIR_INODE} ${DEVICE_DIR}/${IMAGE_NAME})
readonly CASEFOLD_DIR_NONCE=$(get_nonce ${CASEFOLD_DIR_INODE} ${DEVICE_DIR}/${IMAGE_NAME})

echo "Generating Rust file at ${RUST_FILE}..."

cat > "${RUST_FILE}" <<EOF
// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Generated by $(basename "${BASH_SOURCE[0]}").

#![allow(dead_code)]

pub const KEY: &[u8; 64] = &[
    ${KEY_BYTES}
];

pub const UUID: &[u8; 16] = &[
    ${UUID_BYTES}
];

pub const DIR_INODE: u64 = ${DIR_INODE};
pub const CASEFOLD_DIR_INODE: u64 = ${CASEFOLD_DIR_INODE};

pub const DIR_NONCE: &[u8; 16] = &[
    ${DIR_NONCE}
];

pub const CASEFOLD_DIR_NONCE: &[u8; 16] = &[
    ${CASEFOLD_DIR_NONCE}
];

pub struct FileInfo {
    pub unencrypted_name: &'static str,
    pub proxy_name: &'static str,
}

pub const FILES: &[FileInfo] = &[
EOF

echo "${FILE_DATA}" | while IFS=, read -r inode real_name proxy_name; do
  echo "    FileInfo { unencrypted_name: \"${real_name}\", proxy_name: \"${proxy_name}\" }," >> "${RUST_FILE}"
done

echo "];" >> "${RUST_FILE}"

cat >> "${RUST_FILE}" <<EOF

pub const CASEFOLD_FILES: &[FileInfo] = &[
EOF

echo "${CASEFOLD_FILE_DATA}" | while IFS=, read -r inode real_name proxy_name; do
  echo "    FileInfo { unencrypted_name: \"${real_name}\", proxy_name: \"${proxy_name}\" }," >> "${RUST_FILE}"
done

echo "];" >> "${RUST_FILE}"

echo "Done. The rust file is at ${RUST_FILE}"
