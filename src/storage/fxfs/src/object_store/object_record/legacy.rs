// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Contains all legacy data structures and migration code.

use super::*;

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
                Self::EncryptedChild { hash_code: 0, name }
            }
            ObjectKeyDataV40::CasefoldChild { name } => Self::CasefoldChild { name },
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ObjectKeyDataV40 {
    Object,
    Keys,
    Attribute(u64, AttributeKeyV32),
    Child { name: String },
    GraveyardEntry { object_id: u64 },
    Project { project_id: u64, property: ProjectPropertyV32 },
    ExtendedAttribute { name: Vec<u8> },
    GraveyardAttributeEntry { object_id: u64, attribute_id: u64 },
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

pub type ObjectItemV46 = Item<ObjectKeyV43, ObjectValueV46>;
pub type ObjectItemV43 = Item<ObjectKeyV43, ObjectValueV41>;
pub type ObjectItemV41 = Item<ObjectKeyV40, ObjectValueV41>;
pub type ObjectItemV40 = Item<ObjectKeyV40, ObjectValueV40>;

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
