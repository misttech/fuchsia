// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::blob_metadata::{BlobMetadata, BlobMetadataV53};
use crate::lsm_tree::{
    PersistentLayerHeader, PersistentLayerHeaderV39, PersistentLayerInfo, PersistentLayerInfoV39,
};
use crate::object_store::allocator::{
    AllocatorInfo, AllocatorInfoV32, AllocatorKey, AllocatorKeyV32, AllocatorValue,
    AllocatorValueV32,
};
use crate::object_store::journal::super_block::{
    SuperBlockHeader, SuperBlockHeaderV32, SuperBlockRecord, SuperBlockRecordV40,
    SuperBlockRecordV41, SuperBlockRecordV43, SuperBlockRecordV46, SuperBlockRecordV47,
    SuperBlockRecordV49, SuperBlockRecordV50, SuperBlockRecordV54, SuperBlockRecordV55,
    SuperBlockRecordV56,
};
use crate::object_store::journal::{
    JournalRecord, JournalRecordV40, JournalRecordV41, JournalRecordV42, JournalRecordV43,
    JournalRecordV46, JournalRecordV47, JournalRecordV49, JournalRecordV50, JournalRecordV54,
    JournalRecordV55, JournalRecordV56,
};
use crate::object_store::object_record::{
    FsverityMetadata, FsverityMetadataV33, FsverityMetadataV50, ObjectKey, ObjectKeyV40,
    ObjectKeyV43, ObjectKeyV54, ObjectValue, ObjectValueV40, ObjectValueV41, ObjectValueV46,
    ObjectValueV47, ObjectValueV49, ObjectValueV50, ObjectValueV54, ObjectValueV56,
};
use crate::object_store::transaction::{
    Mutation, MutationV40, MutationV41, MutationV43, MutationV46, MutationV47, MutationV49,
    MutationV50, MutationV54, MutationV55, MutationV56,
};
use crate::object_store::{
    EncryptedMutations, EncryptedMutationsV40, EncryptedMutationsV49, StoreInfo, StoreInfoV40,
    StoreInfoV49, StoreInfoV52,
};
use crate::serialized_types::{Version, Versioned, VersionedLatest, versioned_type};
use std::collections::BTreeMap;

/// The latest version of on-disk filesystem format.
///
/// If all layer files are compacted the the journal flushed, and super-block
/// both rewritten, all versions should match this value.
///
/// If making a breaking change, please see EARLIEST_SUPPORTED_VERSION (below).
///
/// IMPORTANT: When changing this (major or minor), update the list of possible versions at
/// https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/cobalt_config/fuchsia/local_storage/versions.txt.
pub const LATEST_VERSION: Version = Version { major: 56, minor: 0 };

/// From this version of the filesystem, the sequence number is removed from the Item struct.
pub const REMOVE_ITEM_SEQUENCE_VERSION: u32 = 55;

/// The earliest supported version of the on-disk filesystem format.
///
/// When a breaking change is made:
/// 1) LATEST_VERSION should have it's major component increased (see above).
/// 2) EARLIEST_SUPPORTED_VERSION should be set to the new LATEST_VERSION.
/// 3) The SuperBlockHeader version (below) should also be set to the new LATEST_VERSION.
///
/// Also check the constant version numbers above for any code cleanup that can happen.
pub const EARLIEST_SUPPORTED_VERSION: Version = Version { major: 40, minor: 0 };

/// From this version of the filesystem, we shrink the size of the extents that are reserved for
/// the superblock and root-parent store to a single block.
pub const SMALL_SUPERBLOCK_VERSION: Version = Version { major: 44, minor: 0 };

/// From this version of the filesystem, the superblock explicitly includes a record for it's
/// first extent. Prior to this, the first extent was assumed based on hard-coded location.
pub const FIRST_EXTENT_IN_SUPERBLOCK_VERSION: Version = Version { major: 45, minor: 0 };

/// This trait prevents types from showing up in `versioned_types` multiple times. `versioned_types`
/// implements this trait for every type passed to it. If a type is listed multiple times then this
/// trait will be implemented for the type multiple times which will fail to compile.
#[allow(dead_code)]
trait UniqueVersionForType {}

macro_rules! versioned_types {
    ( $( $name:ident { $latest:literal.. => $latest_type:ty $(, $major:literal.. => $type:ty )* $(,)? } )+ ) => {
        $(
            static_assertions::assert_type_eq_all!($name, $latest_type);

            versioned_type! {
                $latest.. => $latest_type,
                $( $major.. => $type ),*
            }

            impl UniqueVersionForType for $latest_type {}
            $( impl UniqueVersionForType for $type {} )*
        )+

        pub fn get_type_fingerprints(version: Version) -> BTreeMap<String, String> {
            let mut map = BTreeMap::new();
            $(
                let fingerprint = {
                    let mut fp = None;
                    const FINGERPRINTS: &[(u32, fn() -> String)] = &[
                        ($latest, <$latest_type as fprint::TypeFingerprint>::fingerprint),
                        $( ($major, <$type as fprint::TypeFingerprint>::fingerprint) ),*
                    ];
                    for (major, type_fp) in FINGERPRINTS {
                        if version.major >= *major {
                            fp = Some(type_fp());
                            break;
                        }
                    }
                    fp
                };
                if let Some(fp) = fingerprint {
                    map.insert(stringify!($name).to_string(), fp.to_string());
                }
            )+
            map
        }
    };
}

versioned_types! {
    AllocatorInfo {
        32.. => AllocatorInfoV32,
    }
    AllocatorKey {
        32.. => AllocatorKeyV32,
    }
    AllocatorValue {
        32.. => AllocatorValueV32,
    }
    EncryptedMutations {
        49.. => EncryptedMutationsV49,
        40.. => EncryptedMutationsV40,
    }
    FsverityMetadata {
        50.. => FsverityMetadataV50,
        33.. => FsverityMetadataV33,
    }
    JournalRecord {
        56.. => JournalRecordV56,
        55.. => JournalRecordV55,
        54.. => JournalRecordV54,
        50.. => JournalRecordV50,
        49.. => JournalRecordV49,
        47.. => JournalRecordV47,
        46.. => JournalRecordV46,
        43.. => JournalRecordV43,
        42.. => JournalRecordV42,
        41.. => JournalRecordV41,
        40.. => JournalRecordV40,
    }
    Mutation {
        56.. => MutationV56,
        55.. => MutationV55,
        54.. => MutationV54,
        50.. => MutationV50,
        49.. => MutationV49,
        47.. => MutationV47,
        46.. => MutationV46,
        43.. => MutationV43,
        41.. => MutationV41,
        40.. => MutationV40,
    }
    ObjectKey {
        54.. => ObjectKeyV54,
        43.. => ObjectKeyV43,
        40.. => ObjectKeyV40,
    }
    ObjectValue {
        56.. => ObjectValueV56,
        54.. => ObjectValueV54,
        50.. => ObjectValueV50,
        49.. => ObjectValueV49,
        47.. => ObjectValueV47,
        46.. => ObjectValueV46,
        41.. => ObjectValueV41,
        40.. => ObjectValueV40,
    }
    PersistentLayerHeader {
        39.. => PersistentLayerHeaderV39,
    }
    PersistentLayerInfo {
        39.. => PersistentLayerInfoV39,
    }
    StoreInfo {
        52.. => StoreInfoV52,
        49.. => StoreInfoV49,
        40.. => StoreInfoV40,
    }
    SuperBlockHeader {
        32.. => SuperBlockHeaderV32,
    }
    SuperBlockRecord {
        56.. => SuperBlockRecordV56,
        55.. => SuperBlockRecordV55,
        54.. => SuperBlockRecordV54,
        50.. => SuperBlockRecordV50,
        49.. => SuperBlockRecordV49,
        47.. => SuperBlockRecordV47,
        46.. => SuperBlockRecordV46,
        43.. => SuperBlockRecordV43,
        41.. => SuperBlockRecordV41,
        40.. => SuperBlockRecordV40,
    }
    BlobMetadata {
        53.. => BlobMetadataV53,
    }
}
