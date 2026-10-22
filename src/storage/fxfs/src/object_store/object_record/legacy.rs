// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! When types in `object_record` are versioned, legacy definitions and migration
//! implementations should be placed in this module.

use super::{
    ChildValue, DirTypeV59, EncryptionKeyV59, EncryptionKeysV59, ExtendedAttributeValueV32,
    ExtentValueV38, FsverityMetadataV50, Item, ObjectAttributesV49, ObjectKeyV54, ObjectKindV62,
    ObjectValueV62,
};
use crate::serialized_types::{Migrate, Versioned, migrate_to_version};
use fprint::TypeFingerprint;
use fxfs_crypto::WrappingKeyId;
use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum DirTypeV54 {
    Normal,
    Encrypted(WrappingKeyId),
    LegacyCasefold,
    Casefold,
    EncryptedCasefold(WrappingKeyId),
}

impl From<DirTypeV54> for DirTypeV59 {
    fn from(old: DirTypeV54) -> Self {
        match old {
            DirTypeV54::Normal => DirTypeV59::Normal,
            DirTypeV54::Encrypted(wrapping_key_id) => DirTypeV59::Encrypted(wrapping_key_id.into()),
            DirTypeV54::LegacyCasefold => DirTypeV59::LegacyCasefold,
            DirTypeV54::Casefold => DirTypeV59::Casefold,
            DirTypeV54::EncryptedCasefold(wrapping_key_id) => {
                DirTypeV59::EncryptedCasefold(wrapping_key_id.into())
            }
        }
    }
}

#[derive(Migrate, Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[migrate_to_version(ObjectKindV62)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectKindV59 {
    File {
        refs: u64,
    },
    Directory {
        sub_dirs: u64,
        dir_type: DirTypeV59,
    },
    Graveyard,
    Symlink {
        refs: u64,
        #[serde(with = "crate::zerocopy_serialization")]
        link: Box<[u8]>,
    },
    EncryptedSymlink {
        refs: u64,
        #[serde(with = "crate::zerocopy_serialization")]
        link: Box<[u8]>,
    },
}

#[derive(Migrate, Clone, Debug, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[migrate_to_version(ObjectKindV59)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectKindV54 {
    File {
        refs: u64,
    },
    Directory {
        sub_dirs: u64,
        dir_type: DirTypeV54,
    },
    Graveyard,
    Symlink {
        refs: u64,
        #[serde(with = "crate::zerocopy_serialization")]
        link: Box<[u8]>,
    },
    EncryptedSymlink {
        refs: u64,
        #[serde(with = "crate::zerocopy_serialization")]
        link: Box<[u8]>,
    },
}

fn reject_legacy_key<'de, D: Deserializer<'de>>(_: D) -> Result<fxfs_crypto::FxfsKey, D::Error> {
    Err(SerdeError::custom("LegacyFxfs keys are no longer supported"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum EncryptionKeyV56 {
    LegacyFxfs(#[serde(deserialize_with = "reject_legacy_key")] fxfs_crypto::FxfsKey),
    FscryptInoLblk32File { key_identifier: [u8; 16] },
    FscryptInoLblk32Dir { key_identifier: [u8; 16], nonce: [u8; 16] },
    Fxfs(fxfs_crypto::FxfsKey),
}

impl From<EncryptionKeyV56> for EncryptionKeyV59 {
    fn from(old: EncryptionKeyV56) -> Self {
        match old {
            EncryptionKeyV56::LegacyFxfs(key) => EncryptionKeyV59::LegacyFxfs(key),
            EncryptionKeyV56::FscryptInoLblk32File { key_identifier } => {
                EncryptionKeyV59::FscryptInoLblk32File { key_identifier }
            }
            EncryptionKeyV56::FscryptInoLblk32Dir { key_identifier, nonce } => {
                EncryptionKeyV59::FscryptInoLblk32Dir { key_identifier, nonce }
            }
            EncryptionKeyV56::Fxfs(key) => EncryptionKeyV59::Fxfs(key),
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct EncryptionKeysV56(Vec<(u64, EncryptionKeyV56)>);

impl From<EncryptionKeysV56> for EncryptionKeysV59 {
    fn from(old: EncryptionKeysV56) -> Self {
        Self(old.0.into_iter().map(|(id, key)| (id, key.into())).collect())
    }
}

#[derive(Migrate, Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV62)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectValueV59 {
    None,
    Some,
    Object { kind: ObjectKindV59, attributes: ObjectAttributesV49 },
    Keys(EncryptionKeysV59),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValue),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV50 },
}

pub type ObjectItemV59 = Item<ObjectKeyV54, ObjectValueV59>;

impl From<ObjectItemV59> for super::ObjectItemV62 {
    fn from(item: ObjectItemV59) -> Self {
        Self { key: item.key, value: item.value.into() }
    }
}

#[derive(Migrate, Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV59)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectValueV56 {
    None,
    Some,
    Object { kind: ObjectKindV54, attributes: ObjectAttributesV49 },
    Keys(EncryptionKeysV56),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValue),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV50 },
}

pub type ObjectItemV56 = Item<ObjectKeyV54, ObjectValueV56>;

impl From<ObjectItemV56> for ObjectItemV59 {
    fn from(item: ObjectItemV56) -> Self {
        Self { key: item.key, value: item.value.into() }
    }
}
