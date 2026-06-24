// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use include_bytes_from_working_dir::include_bytes_from_working_dir_env;
use include_str_from_working_dir::include_str_from_working_dir_env;

use vbmeta::{ChainPartitionDescriptor, Descriptor, HashDescriptor, Key, Salt, VBMeta};

const PEM: &str = include_str_from_working_dir_env!("AVB_KEY");
const METADATA: &[u8] = include_bytes_from_working_dir_env!("AVB_METADATA");
const SALT: &str = env!("SALT");
const IMAGE: &[u8] = include_bytes_from_working_dir_env!("IMAGE");
const EXPECTED_VBMETA: &[u8] = include_bytes_from_working_dir_env!("EXPECTED_VBMETA");
const EXPECTED_VBMETA_CHAIN: &[u8] = include_bytes_from_working_dir_env!("EXPECTED_VBMETA_CHAIN");
const PUBLIC_KEY: &[u8] = include_bytes_from_working_dir_env!("PUBLIC_KEY");

#[test]
fn avbtool_comparison() {
    let key = Key::try_new(PEM, METADATA).unwrap();

    let salt_bytes: &[u8] = &hex::decode(SALT).unwrap();
    let salt = Salt::try_from(salt_bytes).unwrap();
    let descriptor = Descriptor::Hash(HashDescriptor::new("zircon", IMAGE, salt));
    let descriptors = vec![descriptor];

    let vbmeta = VBMeta::sign(descriptors, key).unwrap();
    assert_eq!(vbmeta.as_bytes(), EXPECTED_VBMETA);
}

#[test]
fn avbtool_chain_partition_comparison() {
    let key = Key::try_new(PEM, METADATA).unwrap();

    let descriptor = Descriptor::ChainPartition(ChainPartitionDescriptor {
        rollback_index_location: 1,
        partition_name: "system".to_string(),
        public_key: PUBLIC_KEY.to_vec(),
    });
    let descriptors = vec![descriptor];

    let vbmeta = VBMeta::sign(descriptors, key).unwrap();
    assert_eq!(vbmeta.as_bytes(), EXPECTED_VBMETA_CHAIN);
}
