// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Contains all legacy data structures and migration code.

use super::*;
use fxfs_crypto::WrappedKeyBytes;

impl From<ObjectKeyDataV43> for ObjectKeyDataV54 {
    fn from(item: ObjectKeyDataV43) -> Self {
        match item {
            ObjectKeyDataV43::Object => Self::Object,
            ObjectKeyDataV43::Keys => Self::Keys,
            ObjectKeyDataV43::Attribute(a, b) => Self::Attribute(a, b),
            ObjectKeyDataV43::Child { name } => Self::Child { name },
            ObjectKeyDataV43::GraveyardEntry { object_id } => Self::GraveyardEntry { object_id },
            ObjectKeyDataV43::Project { project_id, property } => {
                Self::Project { project_id, property }
            }
            ObjectKeyDataV43::ExtendedAttribute { name } => Self::ExtendedAttribute { name },
            ObjectKeyDataV43::GraveyardAttributeEntry { object_id, attribute_id } => {
                Self::GraveyardAttributeEntry { object_id, attribute_id }
            }
            ObjectKeyDataV43::EncryptedCasefoldChild(c) => Self::EncryptedCasefoldChild(c),
            ObjectKeyDataV43::CasefoldChild { name } => Self::LegacyCasefoldChild(name),
            ObjectKeyDataV43::EncryptedChild(c) => Self::EncryptedChild(c),
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub enum ObjectKeyDataV43 {
    Object,
    Keys,
    Attribute(AttributeId, AttributeKeyV32),
    Child {
        name: String,
    },
    GraveyardEntry {
        object_id: u64,
    },
    Project {
        project_id: ProjectId,
        property: ProjectPropertyV32,
    },
    ExtendedAttribute {
        #[serde(with = "crate::zerocopy_serialization")]
        name: Vec<u8>,
    },
    GraveyardAttributeEntry {
        object_id: u64,
        attribute_id: AttributeId,
    },
    EncryptedCasefoldChild(EncryptedCasefoldChild),
    CasefoldChild {
        name: CasefoldString,
    },
    EncryptedChild(EncryptedChild),
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectKeyV54)]
#[migrate_nodefault]
pub struct ObjectKeyV43 {
    pub object_id: u64,
    pub data: ObjectKeyDataV43,
}

impl From<ObjectKeyDataV40> for ObjectKeyDataV43 {
    fn from(item: ObjectKeyDataV40) -> Self {
        match item {
            ObjectKeyDataV40::Object => Self::Object,
            ObjectKeyDataV40::Keys => Self::Keys,
            ObjectKeyDataV40::Attribute(a, b) => Self::Attribute(a, b),
            ObjectKeyDataV40::Child { name } => Self::Child { name },
            ObjectKeyDataV40::GraveyardEntry { object_id } => Self::GraveyardEntry { object_id },
            ObjectKeyDataV40::Project { project_id, property } => {
                Self::Project { project_id, property }
            }
            ObjectKeyDataV40::ExtendedAttribute { name } => Self::ExtendedAttribute { name },
            ObjectKeyDataV40::GraveyardAttributeEntry { object_id, attribute_id } => {
                Self::GraveyardAttributeEntry { object_id, attribute_id }
            }
            ObjectKeyDataV40::EncryptedChild { name } => {
                Self::EncryptedCasefoldChild(EncryptedCasefoldChild { hash_code: 0, name })
            }
            ObjectKeyDataV40::CasefoldChild { name } => Self::CasefoldChild { name },
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub enum ObjectKeyDataV40 {
    Object,
    Keys,
    Attribute(AttributeId, AttributeKeyV32),
    Child { name: String },
    GraveyardEntry { object_id: u64 },
    Project { project_id: ProjectId, property: ProjectPropertyV32 },
    ExtendedAttribute { name: Vec<u8> },
    GraveyardAttributeEntry { object_id: u64, attribute_id: AttributeId },
    EncryptedChild { name: Vec<u8> },
    CasefoldChild { name: CasefoldString },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectKeyV43)]
#[migrate_nodefault]
pub struct ObjectKeyV40 {
    pub object_id: u64,
    pub data: ObjectKeyDataV40,
}

impl From<ObjectKindV49> for ObjectKindV54 {
    fn from(kind: ObjectKindV49) -> Self {
        match kind {
            ObjectKindV49::File { refs } => ObjectKindV54::File { refs },
            ObjectKindV49::Directory { sub_dirs, wrapping_key_id, casefold } => {
                let dir_type = match (casefold, wrapping_key_id) {
                    (true, Some(id)) => DirType::EncryptedCasefold(id),
                    (true, None) => DirType::LegacyCasefold, // Existing casefold dirs use Legacy
                    (false, Some(id)) => DirType::Encrypted(id),
                    (false, None) => DirType::Normal,
                };
                ObjectKindV54::Directory { sub_dirs, dir_type }
            }
            ObjectKindV49::Graveyard => ObjectKindV54::Graveyard,
            ObjectKindV49::Symlink { refs, link } => ObjectKindV54::Symlink { refs, link },
            ObjectKindV49::EncryptedSymlink { refs, link } => {
                ObjectKindV54::EncryptedSymlink { refs, link }
            }
        }
    }
}

impl From<ObjectKindV46> for ObjectKindV49 {
    fn from(value: ObjectKindV46) -> Self {
        match value {
            ObjectKindV46::File { refs } => Self::File { refs },
            ObjectKindV46::Directory { sub_dirs, wrapping_key_id, casefold } => Self::Directory {
                sub_dirs,
                wrapping_key_id: wrapping_key_id.map(u128::to_le_bytes),
                casefold,
            },
            ObjectKindV46::Graveyard => Self::Graveyard,
            ObjectKindV46::Symlink { refs, link } => Self::Symlink { refs, link: link.into() },
            ObjectKindV46::EncryptedSymlink { refs, link } => {
                Self::EncryptedSymlink { refs, link: link.into() }
            }
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub enum ObjectKindV46 {
    File { refs: u64 },
    Directory { sub_dirs: u64, wrapping_key_id: Option<u128>, casefold: bool },
    Graveyard,
    Symlink { refs: u64, link: Vec<u8> },
    EncryptedSymlink { refs: u64, link: Vec<u8> },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint)]
#[migrate_to_version(ObjectKindV46)]
pub enum ObjectKindV41 {
    File { refs: u64 },
    Directory { sub_dirs: u64, wrapping_key_id: Option<u128>, casefold: bool },
    Graveyard,
    Symlink { refs: u64, link: Vec<u8> },
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub enum ObjectKindV40 {
    File { refs: u64, has_overwrite_extents: bool },
    Directory { sub_dirs: u64, wrapping_key_id: Option<u128>, casefold: bool },
    Graveyard,
    Symlink { refs: u64, link: Vec<u8> },
}

impl From<ObjectKindV40> for ObjectKindV41 {
    fn from(value: ObjectKindV40) -> Self {
        match value {
            // Ignore has_overwrite_extents - it wasn't used here and we are moving it.
            ObjectKindV40::File { refs, .. } => ObjectKindV41::File { refs },
            ObjectKindV40::Directory { sub_dirs, wrapping_key_id, casefold } => {
                ObjectKindV41::Directory { sub_dirs, wrapping_key_id, casefold }
            }
            ObjectKindV40::Graveyard => ObjectKindV41::Graveyard,
            ObjectKindV40::Symlink { refs, link } => ObjectKindV41::Symlink { refs, link },
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint, Versioned)]
pub struct FsverityMetadataV33 {
    pub root_digest: RootDigestV33,
    pub salt: Vec<u8>,
}

impl From<FsverityMetadataV33> for FsverityMetadataV50 {
    fn from(value: FsverityMetadataV33) -> Self {
        Self::Internal(value.root_digest, value.salt)
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub struct FxfsKeyV40 {
    pub wrapping_key_id: u128,
    pub key: WrappedKeyBytes,
}

impl From<FxfsKeyV40> for FxfsKeyV49 {
    fn from(value: FxfsKeyV40) -> Self {
        Self { wrapping_key_id: value.wrapping_key_id.to_le_bytes(), key: value.key }
    }
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint)]
#[migrate_to_version(EncryptionKeyV49)]
pub enum EncryptionKeyV47 {
    Fxfs(FxfsKeyV40),
    FscryptInoLblk32File { key_identifier: [u8; 16] },
    FscryptInoLblk32Dir { key_identifier: [u8; 16], nonce: [u8; 16] },
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub struct EncryptionKeysV47(Vec<(u64, EncryptionKeyV47)>);

impl From<EncryptionKeysV47> for EncryptionKeysV49 {
    fn from(value: EncryptionKeysV47) -> Self {
        Self(value.0.into_iter().map(|(id, key)| (id, key.into())).collect())
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub enum EncryptionKeysV40 {
    AES256XTS(WrappedKeysV40),
}

impl From<EncryptionKeysV40> for EncryptionKeysV47 {
    fn from(EncryptionKeysV40::AES256XTS(WrappedKeysV40(keys)): EncryptionKeysV40) -> Self {
        EncryptionKeysV47(
            keys.into_iter().map(|(id, key)| (id, EncryptionKeyV47::Fxfs(key))).collect(),
        )
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
pub struct WrappedKeysV40(pub Vec<(u64, FxfsKeyV40)>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum EncryptionKeyV49 {
    Fxfs(fxfs_crypto::FxfsKey),
    FscryptInoLblk32File { key_identifier: [u8; 16] },
    FscryptInoLblk32Dir { key_identifier: [u8; 16], nonce: [u8; 16] },
}

impl From<EncryptionKeyV49> for EncryptionKeyV56 {
    fn from(old: EncryptionKeyV49) -> Self {
        match old {
            EncryptionKeyV49::Fxfs(key) => EncryptionKeyV56::LegacyFxfs(key),
            EncryptionKeyV49::FscryptInoLblk32File { key_identifier } => {
                EncryptionKeyV56::FscryptInoLblk32File { key_identifier }
            }
            EncryptionKeyV49::FscryptInoLblk32Dir { key_identifier, nonce } => {
                EncryptionKeyV56::FscryptInoLblk32Dir { key_identifier, nonce }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct EncryptionKeysV49(Vec<(u64, EncryptionKeyV49)>);

impl From<EncryptionKeysV49> for EncryptionKeysV56 {
    fn from(old: EncryptionKeysV49) -> Self {
        Self(old.0.into_iter().map(|(id, key)| (id, key.into())).collect())
    }
}

#[derive(Migrate, Clone, Debug, PartialEq, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV56)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectValueV54 {
    None,
    Some,
    Object { kind: ObjectKindV54, attributes: ObjectAttributesV49 },
    Keys(EncryptionKeysV49),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValue),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV50 },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV50)]
pub enum ObjectValueV49 {
    None,
    Some,
    Object { kind: ObjectKindV49, attributes: ObjectAttributesV49 },
    Keys(EncryptionKeysV49),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValueV32),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV33 },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV49)]
pub enum ObjectValueV47 {
    None,
    Some,
    Object { kind: ObjectKindV46, attributes: ObjectAttributesV32 },
    Keys(EncryptionKeysV47),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValueV32),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV33 },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV47)]
pub enum ObjectValueV46 {
    None,
    Some,
    Object { kind: ObjectKindV46, attributes: ObjectAttributesV32 },
    Keys(EncryptionKeysV40),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValueV32),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV33 },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint, Versioned)]
#[migrate_to_version(ObjectValueV46)]
pub enum ObjectValueV41 {
    None,
    Some,
    Object { kind: ObjectKindV41, attributes: ObjectAttributesV32 },
    Keys(EncryptionKeysV40),
    Attribute { size: u64, has_overwrite_extents: bool },
    Extent(ExtentValueV38),
    Child(ChildValueV32),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV33 },
}

impl From<ObjectValueV40> for ObjectValueV41 {
    fn from(value: ObjectValueV40) -> Self {
        match value {
            ObjectValueV40::None => ObjectValueV41::None,
            ObjectValueV40::Some => ObjectValueV41::Some,
            ObjectValueV40::Object { kind, attributes } => {
                ObjectValueV41::Object { kind: kind.into(), attributes }
            }
            ObjectValueV40::Keys(keys) => ObjectValueV41::Keys(keys),
            ObjectValueV40::Attribute { size } => {
                ObjectValueV41::Attribute { size, has_overwrite_extents: false }
            }
            ObjectValueV40::Extent(extent_value) => ObjectValueV41::Extent(extent_value),
            ObjectValueV40::Child(child) => ObjectValueV41::Child(child),
            ObjectValueV40::Trim => ObjectValueV41::Trim,
            ObjectValueV40::BytesAndNodes { bytes, nodes } => {
                ObjectValueV41::BytesAndNodes { bytes, nodes }
            }
            ObjectValueV40::ExtendedAttribute(xattr) => ObjectValueV41::ExtendedAttribute(xattr),
            ObjectValueV40::VerifiedAttribute { size, fsverity_metadata } => {
                ObjectValueV41::VerifiedAttribute { size, fsverity_metadata }
            }
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint, Versioned)]
pub enum ObjectValueV40 {
    None,
    Some,
    Object { kind: ObjectKindV40, attributes: ObjectAttributesV32 },
    Keys(EncryptionKeysV40),
    Attribute { size: u64 },
    Extent(ExtentValueV38),
    Child(ChildValueV32),
    Trim,
    BytesAndNodes { bytes: i64, nodes: i64 },
    ExtendedAttribute(ExtendedAttributeValueV32),
    VerifiedAttribute { size: u64, fsverity_metadata: FsverityMetadataV33 },
}

#[derive(Migrate, Serialize, Deserialize, TypeFingerprint)]
#[migrate_to_version(ObjectAttributesV49)]
pub struct ObjectAttributesV32 {
    pub creation_time: TimestampV32,
    pub modification_time: TimestampV32,
    #[serde(with = "crate::object_store::project_id::optional_project_id")]
    pub project_id: Option<ProjectId>,
    pub posix_attributes: Option<PosixAttributesV32>,
    pub allocated_size: u64,
    pub access_time: TimestampV32,
    pub change_time: TimestampV32,
}

impl From<TimestampV32> for TimestampV49 {
    fn from(timestamp: TimestampV32) -> Self {
        Self::from_secs_and_nanos(timestamp.secs, timestamp.nanos)
    }
}

#[derive(Copy, Clone, Serialize, Deserialize, TypeFingerprint)]
pub struct TimestampV32 {
    pub secs: u64,
    pub nanos: u32,
}

pub type ObjectItemV54 = LegacyItem<ObjectKeyV54, ObjectValueV54>;
pub type ObjectItemV55 = Item<ObjectKeyV54, ObjectValueV54>;

impl From<ObjectItemV54> for ObjectItemV55 {
    fn from(item: ObjectItemV54) -> Self {
        Self { key: item.key, value: item.value }
    }
}

impl From<ObjectItemV55> for ObjectItemV56 {
    fn from(item: ObjectItemV55) -> Self {
        Self { key: item.key, value: item.value.into() }
    }
}

pub type ObjectItemV49 = LegacyItem<ObjectKeyV43, ObjectValueV49>;
pub type ObjectItemV47 = LegacyItem<ObjectKeyV43, ObjectValueV47>;
pub type ObjectItemV46 = LegacyItem<ObjectKeyV43, ObjectValueV46>;
pub type ObjectItemV43 = LegacyItem<ObjectKeyV43, ObjectValueV41>;
pub type ObjectItemV41 = LegacyItem<ObjectKeyV40, ObjectValueV41>;
pub type ObjectItemV40 = LegacyItem<ObjectKeyV40, ObjectValueV40>;

impl From<ObjectItemV50> for ObjectItemV54 {
    fn from(item: ObjectItemV50) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}

impl From<ObjectItemV49> for ObjectItemV50 {
    fn from(item: ObjectItemV49) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}
impl From<ObjectItemV47> for ObjectItemV49 {
    fn from(item: ObjectItemV47) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}
impl From<ObjectItemV46> for ObjectItemV47 {
    fn from(item: ObjectItemV46) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}
impl From<ObjectItemV43> for ObjectItemV46 {
    fn from(item: ObjectItemV43) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}
impl From<ObjectItemV41> for ObjectItemV43 {
    fn from(item: ObjectItemV41) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}
impl From<ObjectItemV40> for ObjectItemV41 {
    fn from(item: ObjectItemV40) -> Self {
        Self { key: item.key.into(), value: item.value.into(), sequence: item.sequence }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_v32_to_v49() {
        let v32 = TimestampV32 { secs: 100, nanos: 200 };
        let v49: TimestampV49 = v32.into();
        assert_eq!(v32.secs * 1_000_000_000 + v32.nanos as u64, v49.nanos);
    }
}
