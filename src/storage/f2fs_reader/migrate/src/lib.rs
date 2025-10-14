// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Error, bail, ensure};
use f2fs_reader::{
    AdviseFlags, BLOCK_SIZE as F2FS_BLOCK_SIZE, F2fsReader, FileType, Flags, FsVerityDescriptor,
    InlineFlags, Inode, NEW_ADDR, NULL_ADDR, XattrIndex,
};
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem};
use fxfs::object_handle::{ObjectHandle, ObjectProperties, ReadObjectHandle};
use fxfs::object_store::journal::BLOCK_SIZE as FXFS_BLOCK_SIZE;
use fxfs::object_store::journal::super_block::SuperBlockInstance;
use fxfs::object_store::transaction::{LockKey, Mutation, Options, Transaction, lock_keys};
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{
    AttributeKey, DEFAULT_DATA_ATTRIBUTE_ID, DataObjectHandle, Directory, ExtentValue,
    FSCRYPT_KEY_ID, HandleOptions, NewChildStoreOptions, ObjectAttributes, ObjectDescriptor,
    ObjectKey, ObjectKind, ObjectStore, ObjectValue, PosixAttributes, StoreOptions, Timestamp,
    VOLUME_DATA_KEY_ID,
};
use fxfs_crypto::{Crypt, EncryptionKey, WrappingKeyId};
use std::collections::HashSet;
use std::sync::Arc;
use storage_device::DeviceHolder;

mod ranged_device;
use crate::ranged_device::RangedDevice;

#[cfg(test)]
mod integration_test;

const PLACEHOLDER_VERITY_ATTR: u64 = 513; // Just out of the xattr range.

fn inode_to_object_attributes(inode: &Inode, allocated_size: u64) -> ObjectAttributes {
    let mode = inode.header.mode;
    ObjectAttributes {
        creation_time: Timestamp::from_secs_and_nanos(inode.header.ctime, inode.header.ctime_nanos),
        modification_time: Timestamp::from_secs_and_nanos(
            inode.header.mtime,
            inode.header.mtime_nanos,
        ),
        project_id: 0,
        posix_attributes: Some(PosixAttributes {
            mode: mode.bits() as u32,
            uid: inode.header.uid,
            gid: inode.header.gid,
            rdev: 0,
        }),
        allocated_size,
        access_time: Timestamp::from_secs_and_nanos(inode.header.atime, inode.header.atime_nanos),
        change_time: Timestamp::from_secs_and_nanos(inode.header.ctime, inode.header.ctime_nanos),
    }
}

/// Helper to move xattr from `inode` to object with `object_id`, handling special cases.
fn migrate_xattr(
    inode: &Inode,
    object_id: u64,
    store: &ObjectStore,
    transaction: &mut Transaction<'_>,
) -> Result<(), Error> {
    for xattr in &inode.xattr {
        match xattr.index {
            XattrIndex::User => {
                ensure!(
                    xattr.name.len() < 9 || &xattr.name[..9] != b"security.",
                    "illegal user-provided security context"
                );
                transaction.add(
                    store.store_object_id(),
                    Mutation::replace_or_insert_object(
                        ObjectKey::extended_attribute(object_id, xattr.name.to_vec()),
                        ObjectValue::inline_extended_attribute(xattr.value.to_vec()),
                    ),
                );
            }
            XattrIndex::Encryption => {
                // This is interpreted via inode.context. We can ignore this xattr.
                ensure!(&*xattr.name == b"c", "unexpected encryption xattr {:?}", xattr.name);
            }
            XattrIndex::Security => {
                ensure!(
                    &*xattr.name == b"selinux" || &*xattr.name == b"sehash",
                    "unexpected security xattr {:?}",
                    xattr.name
                );
                // TODO(https://fxbug.dev/450104899): Ensure that 'security.sehash' is also treated as 'security' xattr namespace.
                let mut name: Vec<u8> = b"security.".into();
                name.extend_from_slice(&xattr.name);
                transaction.add(
                    store.store_object_id(),
                    Mutation::replace_or_insert_object(
                        ObjectKey::extended_attribute(object_id, name),
                        ObjectValue::inline_extended_attribute(xattr.value.to_vec()),
                    ),
                );
            }
            XattrIndex::PosixAclDefault | XattrIndex::PosixAclAccess => {
                // TODO(https://fxbug.dev/450498061): Support this.
            }
            XattrIndex::Verity => {
                ensure!(&*xattr.name == b"v", "unexpected verity xattr {:?}", xattr.name);
                ensure!(xattr.value.len() == 16, "Verity xattr size");
                ensure!(
                    u32::from_le_bytes(xattr.value[0..4].try_into().unwrap()) == 1,
                    "Unknown verity xattr version"
                );
                ensure!(
                    u32::from_le_bytes(xattr.value[4..8].try_into().unwrap()) == 256,
                    "Expected 256 byte descriptor size."
                );
                // Offset of the descriptor. It should be past the start of the verity data.
                ensure!(
                    u64::from_le_bytes(xattr.value[8..16].try_into().unwrap())
                        >= FsVerityDescriptor::offset_from_size(inode.header.size),
                    "Unexpected offset for verity descriptor"
                );
            }
            _ => {
                panic!("Unexpected xattr {xattr:?}");
            }
        }
    }
    Ok(())
}

/// Helper to set the appropriate key type based on fscrypt context.
/// Returns (wrapping_key_id, key_id, keys)
async fn keys_from_context(
    object_id: u64,
    context: &Option<fscrypt::Context>,
    owner: &Directory<ObjectStore>,
    parent_is_fscrypt: bool,
    is_file: bool,
) -> Result<(Option<WrappingKeyId>, u64, Vec<(u64, EncryptionKey)>), Error> {
    if let Some(context) = context {
        ensure!(context.flags & fscrypt::POLICY_FLAGS_PAD_16 != 0, "require 16 byte padding");
        Ok((
            Some([0; 16]),  // Presence of wrapping_key_id implies fscrypt. Value irrelevant.
            FSCRYPT_KEY_ID, // fscrypt always uses key_id = 1
            if context.flags & fscrypt::POLICY_FLAGS_INO_LBLK_32 != 0 {
                if is_file {
                    vec![(
                        FSCRYPT_KEY_ID,
                        EncryptionKey::FscryptInoLblk32File {
                            key_identifier: context.main_key_identifier,
                        },
                    )]
                } else {
                    vec![(
                        FSCRYPT_KEY_ID,
                        EncryptionKey::FscryptInoLblk32Dir {
                            key_identifier: context.main_key_identifier,
                            nonce: context.nonce,
                        },
                    )]
                }
            } else {
                bail!("Unsupported fscrypt encryption policy.");
            },
        ))
    } else {
        let store = owner.store();
        let crypt = store.crypt().unwrap();
        let (key, _unwrapped_key) =
            crypt.create_key(object_id, fxfs_crypto::KeyPurpose::Data).await.unwrap();
        Ok((
            if parent_is_fscrypt { Some([0; 16]) } else { None },
            VOLUME_DATA_KEY_ID,
            vec![(VOLUME_DATA_KEY_ID, EncryptionKey::Fxfs(key))],
        ))
    }
}

/// Migrates f2fs nodes to fxfs.
///
/// We preserve inode mappings (to object_id), attributes, xattr -- basically everything we can.
/// Some of these things are not easily achievable with standard fxfs interfaces like 'add_child'
/// so much of this work has to be done at the raw transaction/mutation level.
///
/// `offset` specifies where the f2fs file system starts - typically 0 but may differ
///   if migrating across partition boundaries.
/// `existing_inodes` is used to handle hard links.
/// `f2fs_metadata_blocks` must be preserved to ensure that the resulting image is still parsable
/// as a valid f2fs image.
pub async fn migrate(
    offset: u64,
    f2fs: &F2fsReader,
    fxfs: &mut OpenFxFilesystem,
    ino: u32,
    dir: Directory<ObjectStore>,
    files_to_copy: &mut HashSet<u64>,
    f2fs_metadata_blocks: &mut HashSet<u32>,
) -> Result<(), Error> {
    assert_eq!(
        F2FS_BLOCK_SIZE as u64, FXFS_BLOCK_SIZE,
        "We currently assume block sizes are the same."
    );
    let mut existing_inodes = HashSet::new();

    let mut stack = vec![(ino, dir)];
    while let Some((ino, dir)) = stack.pop() {
        // Any dentry blocks for this directory are f2fs metadata.
        let inode = f2fs.read_inode(ino).await?;
        for addr in &inode.block_addrs {
            f2fs_metadata_blocks.insert(*addr);
        }
        for (_, addr) in inode.data_blocks() {
            f2fs_metadata_blocks.insert(addr);
        }

        for entry in f2fs.readdir(ino).await? {
            let object_id = entry.ino as u64;
            let inode = f2fs.read_inode(entry.ino).await?;
            let flags = inode.header.flags;
            let casefold = flags.contains(Flags::Casefold);

            let mut transaction = fxfs
                .clone()
                .new_transaction(
                    lock_keys![
                        LockKey::object(dir.owner().store_object_id(), dir.object_id()),
                        LockKey::object(dir.owner().store_object_id(), object_id)
                    ],
                    Options::default(),
                )
                .await?;

            let (wrapping_key_id, key_id, keys) = keys_from_context(
                object_id,
                &inode.context,
                &dir,
                dir.wrapping_key_id().is_some(),
                entry.file_type == FileType::RegularFile,
            )
            .await?;

            transaction.add(
                dir.owner().store_object_id(),
                Mutation::replace_or_insert_object(
                    ObjectKey::keys(object_id),
                    ObjectValue::Keys(keys.into()),
                ),
            );

            if !existing_inodes.insert(entry.ino) {
                // Hard link to an existing inode.
                ensure!(entry.file_type == FileType::RegularFile, "Hard link to non-file");
                if wrapping_key_id.is_some() {
                    transaction.add(
                        dir.store().store_object_id(),
                        Mutation::replace_or_insert_object(
                            ObjectKey::encrypted_child(
                                dir.object_id(),
                                entry.raw_filename,
                                if casefold { entry.hash_code } else { 0 },
                            ),
                            ObjectValue::child(object_id, ObjectDescriptor::File),
                        ),
                    );
                } else {
                    transaction.add(
                        dir.store().store_object_id(),
                        Mutation::replace_or_insert_object(
                            ObjectKey::child(dir.object_id(), &entry.filename, casefold),
                            ObjectValue::child(object_id, ObjectDescriptor::File),
                        ),
                    );
                }
                dir.store().adjust_refs(&mut transaction, object_id, 1).await?;
                transaction.commit().await?;
                continue;
            }

            // Both directories and files can have xattr.
            migrate_xattr(&inode, object_id, dir.store(), &mut transaction)?;

            match entry.file_type {
                FileType::Directory => {
                    transaction.add(
                        dir.owner().store_object_id(),
                        Mutation::insert_object(
                            ObjectKey::object(object_id),
                            ObjectValue::Object {
                                kind: ObjectKind::Directory {
                                    sub_dirs: 0,
                                    casefold,
                                    wrapping_key_id,
                                },
                                attributes: inode_to_object_attributes(&inode, 0),
                            },
                        ),
                    );
                    if dir.wrapping_key_id().is_some() {
                        transaction.add(
                            dir.store().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::encrypted_child(
                                    dir.object_id(),
                                    entry.raw_filename,
                                    if casefold { entry.hash_code } else { 0 },
                                ),
                                ObjectValue::child(object_id, ObjectDescriptor::Directory),
                            ),
                        );
                    } else {
                        transaction.add(
                            dir.store().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::child(dir.object_id(), &entry.filename, casefold),
                                ObjectValue::child(object_id, ObjectDescriptor::Directory),
                            ),
                        );
                    }

                    // Bump sub_dirs count in parent.
                    let mut mutation =
                        dir.store().get_object_mutation(&transaction, dir.object_id()).await?;
                    if let ObjectValue::Object {
                        kind: ObjectKind::Directory { sub_dirs, .. },
                        ..
                    } = &mut mutation.item.value
                    {
                        *sub_dirs = sub_dirs.saturating_add_signed(1);
                    } else {
                        bail!("Parent is not a directory");
                    };
                    transaction.add(dir.store().store_object_id(), Mutation::ObjectStore(mutation));

                    transaction.commit().await?;
                    let new_dir = Directory::open_unchecked(
                        dir.owner().clone(),
                        object_id,
                        wrapping_key_id,
                        casefold,
                    );
                    stack.push((entry.ino, new_dir));
                }
                FileType::RegularFile => {
                    // Add inode block and related blocks to set of f2fs metadata blocks.
                    for addr in &inode.block_addrs {
                        f2fs_metadata_blocks.insert(*addr);
                    }

                    let verity_offset = if inode.header.advise_flags.contains(AdviseFlags::Verity) {
                        // TODO(https://fxbug.dev/399727919): Handle the rest of verity migration.
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::insert_object(
                                ObjectKey::attribute(
                                    object_id,
                                    PLACEHOLDER_VERITY_ATTR,
                                    AttributeKey::Attribute,
                                ),
                                ObjectValue::attribute(1024 * 1024 * 1024, false),
                            ),
                        );
                        Some(FsVerityDescriptor::offset_from_size(inode.header.size))
                    } else {
                        None
                    };

                    let mut allocated_size = 0;
                    let inline_flags = inode.header.inline_flags;
                    //let mut mutation_count = 4;
                    if inline_flags.contains(InlineFlags::Data) {
                        // Marking a file for verity moves the data out of inline.
                        assert!(verity_offset.is_none());
                        if inode.header.size > 0 {
                            // We have to allocate inline files.
                            // Encrypted inline files are not possible, so this is relatively uncommon.
                            files_to_copy.insert(object_id);
                            allocated_size = F2FS_BLOCK_SIZE as u64;
                        }
                    } else if inode.context.is_some() {
                        // Fscrypt file, extents are remapped.
                        for (block_offset, block_addr) in inode.data_blocks() {
                            let device_range = offset + block_addr as u64 * F2FS_BLOCK_SIZE as u64
                                ..offset + (block_addr as u64 + 1) * F2FS_BLOCK_SIZE as u64;
                            let mut logical_range = block_offset as u64 * F2FS_BLOCK_SIZE as u64
                                ..(block_offset as u64 + 1) * F2FS_BLOCK_SIZE as u64;
                            let attr_id = match verity_offset {
                                Some(verity_offset) if logical_range.start >= verity_offset => {
                                    logical_range.start -= verity_offset;
                                    logical_range.end -= verity_offset;
                                    PLACEHOLDER_VERITY_ATTR
                                }
                                _ => DEFAULT_DATA_ATTRIBUTE_ID,
                            };
                            dir.store().mark_allocated(
                                &mut transaction,
                                dir.store().store_object_id(),
                                device_range.clone(),
                            )?;
                            transaction.add(
                                dir.store().store_object_id(),
                                Mutation::merge_object(
                                    ObjectKey::extent(object_id, attr_id, logical_range),
                                    ObjectValue::Extent(ExtentValue::new_raw(
                                        device_range.start,
                                        key_id,
                                    )),
                                ),
                            );
                            allocated_size += F2FS_BLOCK_SIZE as u64;
                        }
                    } else {
                        // Default encrypted file, data will be copied later.
                        files_to_copy.insert(object_id);
                        allocated_size = if verity_offset.is_some() {
                            // Don't count the data blocks after EOF. Those are for verity.
                            let limit = inode.header.size.div_ceil(F2FS_BLOCK_SIZE as u64);
                            inode
                                .data_blocks()
                                .take_while(|(offset, _)| (*offset as u64) < limit)
                                .count() as u64
                                * F2FS_BLOCK_SIZE as u64
                        } else {
                            // Max blocks per file is ~1B and block count is stored in a u32 so
                            // there is basically no chance of this overflowing.
                            inode.data_blocks().count() as u64 * F2FS_BLOCK_SIZE as u64
                        };
                    }

                    transaction.add(
                        dir.owner().store_object_id(),
                        Mutation::insert_object(
                            ObjectKey::object(object_id),
                            ObjectValue::Object {
                                kind: ObjectKind::File { refs: 1 },
                                attributes: inode_to_object_attributes(&inode, allocated_size),
                            },
                        ),
                    );
                    transaction.add(
                        dir.owner().store_object_id(),
                        Mutation::insert_object(
                            ObjectKey::attribute(
                                object_id,
                                DEFAULT_DATA_ATTRIBUTE_ID,
                                AttributeKey::Attribute,
                            ),
                            ObjectValue::attribute(inode.header.size, false),
                        ),
                    );
                    if inode.context.is_some() {
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::encrypted_child(
                                    dir.object_id(),
                                    entry.raw_filename,
                                    if casefold { entry.hash_code } else { 0 },
                                ),
                                ObjectValue::child(object_id, ObjectDescriptor::File),
                            ),
                        );
                    } else {
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::child(dir.object_id(), &entry.filename, casefold),
                                ObjectValue::child(object_id, ObjectDescriptor::File),
                            ),
                        );
                    }
                    transaction.commit().await?;
                }
                FileType::Symlink => {
                    // Add inode block and related blocks to set of f2fs metadata blocks.
                    for addr in &inode.block_addrs {
                        f2fs_metadata_blocks.insert(*addr);
                    }

                    // Symlinks are stored as inline data.
                    let Some(filename) = &inode.inline_data else {
                        bail!("Symlink missing inline data");
                    };
                    let mut filename = filename.to_vec();

                    let object_attributes = inode_to_object_attributes(&inode, 0);
                    if inode.context.is_some() {
                        // Redundant 2-byte length prefix on encrypted symlinks (use
                        // inline_data.len()).
                        filename.drain(..2);
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::encrypted_child(
                                    dir.object_id(),
                                    entry.raw_filename.clone(),
                                    if casefold { entry.hash_code } else { 0 },
                                ),
                                ObjectValue::child(object_id, ObjectDescriptor::Symlink),
                            ),
                        );
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::insert_object(
                                ObjectKey::object(object_id),
                                ObjectValue::encrypted_symlink(
                                    filename,
                                    object_attributes.creation_time,
                                    object_attributes.modification_time,
                                    object_attributes.project_id,
                                ),
                            ),
                        );
                    } else {
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::replace_or_insert_object(
                                ObjectKey::child(dir.object_id(), &entry.filename, casefold),
                                ObjectValue::child(object_id, ObjectDescriptor::Symlink),
                            ),
                        );
                        transaction.add(
                            dir.owner().store_object_id(),
                            Mutation::insert_object(
                                ObjectKey::object(object_id),
                                ObjectValue::symlink(
                                    filename,
                                    object_attributes.creation_time,
                                    object_attributes.modification_time,
                                    object_attributes.project_id,
                                ),
                            ),
                        );
                    }
                    transaction.commit().await?;
                }
                FileType::Socket => {
                    // We just ignore sockets. They don't really make sense across reboots.
                }
                _ => unimplemented!(),
            }
        }
    }
    Ok(())
}

pub async fn verify(
    f2fs: &F2fsReader,
    fxfs: &OpenFxFilesystem,
    ino: u32,
    dir: Directory<ObjectStore>,
    check_file_contents: bool,
) -> Result<(), Error> {
    let mut stack = vec![(ino, dir)];
    while let Some((ino, dir)) = stack.pop() {
        for entry in f2fs.readdir(ino).await? {
            let object_id = entry.ino as u64;
            let inode = f2fs.read_inode(entry.ino).await.unwrap();
            let flags = inode.header.flags;
            let casefold = flags.contains(Flags::Casefold);
            let mut wrapping_key_id = dir.wrapping_key_id();

            // If f2fs inode has a context, we have an fscrypt file. In fxfs this is marked by the
            // presence of a wrapping_key_id.
            if inode.context.is_some() {
                wrapping_key_id = Some([0; 16]);
            }

            // TODO(https://fxbug.dev/393449584): Lookup and compare fxfs filename.

            match entry.file_type {
                FileType::Directory => {
                    let dir = Directory::open_unchecked(
                        dir.owner().clone(),
                        object_id,
                        wrapping_key_id,
                        casefold,
                    );

                    for xattr in &inode.xattr {
                        match xattr.index {
                            XattrIndex::User => {
                                let fxfs_xattr_value = dir
                                    .get_extended_attribute(xattr.name.to_vec())
                                    .await
                                    .context("xattr read")?;
                                assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
                            }
                            XattrIndex::Security => {
                                let fxfs_xattr_value = dir
                                    .get_extended_attribute("security.selinux".into())
                                    .await
                                    .context("xattr read")?;
                                assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
                            }
                            _ => {}
                        }
                    }

                    let fxfs_properties = dir.get_properties().await.context("get_properties")?;
                    let object_attributes = inode_to_object_attributes(&inode, 0);
                    let f2fs_properties = ObjectProperties {
                        refs: 1,
                        allocated_size: 0,
                        data_attribute_size: 0,
                        creation_time: object_attributes.creation_time,
                        modification_time: object_attributes.modification_time,
                        access_time: object_attributes.access_time,
                        change_time: object_attributes.change_time,
                        sub_dirs: inode.header.links as u64 - 2,
                        posix_attributes: object_attributes.posix_attributes,
                        casefold,
                        wrapping_key_id,
                    };
                    let h = inode.header;
                    assert_eq!(
                        fxfs_properties, f2fs_properties,
                        "entry {entry:?}, inode header: {h:?}"
                    );

                    stack.push((entry.ino, dir));
                }
                FileType::RegularFile => {
                    let handle = ObjectStore::open_object(
                        &dir.owner(),
                        object_id,
                        HandleOptions::default(),
                        None,
                    )
                    .await
                    .context("open object")?;

                    for xattr in &inode.xattr {
                        match xattr.index {
                            XattrIndex::User => {
                                let fxfs_xattr_value = handle
                                    .get_extended_attribute(xattr.name.to_vec())
                                    .await
                                    .context("xattr read")?;
                                assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
                            }
                            XattrIndex::Security => {
                                let fxfs_xattr_value = dir
                                    .get_extended_attribute("security.selinux".into())
                                    .await
                                    .context("xattr read")?;
                                assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
                            }
                            _ => {}
                        }
                    }

                    let fxfs_properties =
                        handle.get_properties().await.context("get properties")?;
                    let f2fs_allocated_size = if let Some(data) = inode.inline_data.as_ref() {
                        if data.len() > 0 { F2FS_BLOCK_SIZE as u64 } else { 0 }
                    } else {
                        (if let Some((options, root)) =
                            handle.get_descriptor().expect("Requesting fsverity info")
                        {
                            // When fsverity is enabled, ignore the local trailing blocks for the
                            // merkle data and descriptor. Instead compare as if they used the same
                            // bytes for merkle storage.
                            let descriptor = FsVerityDescriptor::from_verification_options(
                                &options,
                                &root,
                                inode.header.size,
                            )
                            .unwrap();
                            let limit = inode.header.size.div_ceil(F2FS_BLOCK_SIZE as u64);
                            inode
                                .data_blocks()
                                .take_while(|(offset, _)| (*offset as u64) < limit)
                                .count() as u64
                                + descriptor.leaf_node_size_fxfs().div_ceil(FXFS_BLOCK_SIZE as u64)
                        } else {
                            inode.data_blocks().count() as u64
                        }) * F2FS_BLOCK_SIZE as u64
                    };
                    let object_attributes = inode_to_object_attributes(&inode, f2fs_allocated_size);
                    let f2fs_properties = ObjectProperties {
                        refs: inode.header.links as u64,
                        allocated_size: object_attributes.allocated_size,
                        data_attribute_size: inode.header.size,
                        creation_time: object_attributes.creation_time,
                        modification_time: object_attributes.modification_time,
                        access_time: object_attributes.access_time,
                        change_time: object_attributes.change_time,
                        sub_dirs: 0,
                        posix_attributes: object_attributes.posix_attributes,
                        casefold,
                        wrapping_key_id: None,
                    };
                    assert_eq!(fxfs_properties, f2fs_properties, "{}", entry.filename);

                    if check_file_contents {
                        let inline_flags = inode.header.inline_flags;
                        if inline_flags.contains(InlineFlags::Data) {
                            let mut buffer = handle.allocate_buffer(FXFS_BLOCK_SIZE as usize).await;
                            let len = handle.read(0, buffer.as_mut()).await.context("read")?;
                            let f2fs_block = inode.inline_data.as_ref().unwrap();
                            assert_eq!(
                                &buffer.as_slice()[..len],
                                f2fs_block.as_ref(),
                                "Inline data mismatch."
                            );
                        } else {
                            let device = fxfs.device();
                            let mut fxfs_buffer =
                                device.allocate_buffer(FXFS_BLOCK_SIZE as usize).await;
                            for i in 0..inode.header.block_size as u32 {
                                if let Some(f2fs_block) = f2fs.read_data(&inode, i).await.unwrap() {
                                    let len = handle
                                        .read(
                                            i as u64 * FXFS_BLOCK_SIZE as u64,
                                            fxfs_buffer.as_mut(),
                                        )
                                        .await
                                        .unwrap();
                                    assert_eq!(
                                        &fxfs_buffer.as_slice()[..len],
                                        &f2fs_block.as_slice()[..len],
                                        "File content mismatch for ino {}",
                                        object_id
                                    );
                                }
                            }
                        }
                    }

                    // Verify that the root digest matches when the verity bit is set.
                    // TODO(https://fxbug.dev/399727919): Trigger this branch when the f2fs flags
                    // are set once all the f2fs migrations are done.
                    if let Some((fxfs_descriptor, fxfs_root)) =
                        handle.get_descriptor().expect("Requesting descriptor")
                    {
                        assert!(inode.header.advise_flags.contains(AdviseFlags::Verity));
                        let (descriptor_block, _) =
                            inode.data_blocks().last().expect("Must have a data block for verity");
                        let descriptor_data =
                            f2fs.read_data(&inode, descriptor_block).await.unwrap().unwrap();
                        let f2fs_descriptor =
                            FsVerityDescriptor::from_bytes(descriptor_data.as_slice())
                                .expect("Validating descriptor");
                        assert_eq!(fxfs_root.as_slice(), f2fs_descriptor.root);
                        assert_eq!(
                            fxfs_descriptor.salt.unwrap_or_default().as_slice(),
                            f2fs_descriptor.salt
                        );
                    }
                }
                FileType::Symlink => {
                    if check_file_contents {
                        let f2fs_link = f2fs.read_symlink(&inode)?;
                        let fxfs_link = dir
                            .store()
                            .read_symlink(object_id)
                            .await
                            .context("failed to read fxfs symlink")?;
                        assert_eq!(
                            f2fs_link.as_ref(),
                            &fxfs_link,
                            "Symlink differs for inode {:?}",
                            inode.context
                        );
                    }
                }
                _ => unimplemented!(),
            }
        }
    }
    Ok(())
}

/// Reserves disk regions in fxfs to ensure that we don't overwrite critical f2fs metadata.
/// `offset` specifies where the f2fs file system starts - typically 0 but may differ
///   if migrating across partition boundaries.
pub async fn reserve_f2fs_metadata<'a>(
    offset: u64,
    f2fs: &F2fsReader,
    f2fs_main_blkaddr: u32, // Start of the 'data' region.
    blocks: &HashSet<u32>,
    files_to_copy: &HashSet<u64>,
    transaction: &mut Transaction<'a>,
    handle: &'a DataObjectHandle<ObjectStore>,
) -> Result<(), Error> {
    let sb_a = SuperBlockInstance::A.first_extent();
    let sb_b = SuperBlockInstance::B.first_extent();
    let f2fs_metadata_end = offset + f2fs_main_blkaddr as u64 * F2FS_BLOCK_SIZE as u64;

    // F2FS metadata is at the start of the partition. Fxfs superblocks are also at the start.
    // We assume f2fs_main_blkaddr is large enough that f2fs metadata covers both Fxfs superblocks.
    // We reserve the f2fs metadata region, excluding Fxfs superblocks.

    // 1. Region before SB A.
    if offset < sb_a.start {
        let range = offset..sb_a.start;
        handle.extend(transaction, range).await.context("extend before sb_a")?;
    }

    // 2. Region between SB A and SB B.
    let start = std::cmp::max(offset, sb_a.end);
    if start < sb_b.start {
        let range = start..sb_b.start;
        handle.extend(transaction, range).await.context("extend between sb_a and sb_b")?;
    }

    // 3. Region after SB B.
    let start = std::cmp::max(offset, sb_b.end);
    // Assumption: f2fs_metadata_end > sb_b.end
    if start < f2fs_metadata_end {
        let range = start..f2fs_metadata_end;
        handle.extend(transaction, range).await.context("extend after sb_b")?;
    }

    for &block in blocks {
        let byte_range = (offset + block as u64 * F2FS_BLOCK_SIZE as u64)
            ..(offset + (block as u64 + 1) * F2FS_BLOCK_SIZE as u64);
        handle.extend(transaction, byte_range).await.context("extend c")?;
    }
    // TODO(https://fxbug.dev/394701234): We need to ensure that this works with a lot of files and very large
    // files.
    for ino in files_to_copy {
        let inode = f2fs.read_inode(*ino as u32).await?;
        for (_block_offset, block_addr) in inode.data_blocks() {
            let byte_range = (offset + block_addr as u64 * F2FS_BLOCK_SIZE as u64)
                ..(offset + (block_addr as u64 + 1) * F2FS_BLOCK_SIZE as u64);
            handle.extend(transaction, byte_range).await.context("extend d")?;
        }
    }
    Ok(())
}

pub async fn deep_copy_files(
    offset: u64,
    f2fs: &F2fsReader,
    fxfs: &mut OpenFxFilesystem,
    vol: Arc<ObjectStore>,
    files_to_copy: HashSet<u64>,
) -> Result<(), Error> {
    for object_id in files_to_copy {
        let inode = f2fs.read_inode(object_id as u32).await?;
        let object = ObjectStore::open_object(
            &vol,
            object_id,
            HandleOptions::default(),
            vol.crypt().clone(),
        )
        .await?;
        if inode.header.inline_flags.contains(InlineFlags::Data) {
            let len = inode.inline_data.as_ref().unwrap().len();
            let mut buffer = object.allocate_buffer(FXFS_BLOCK_SIZE as usize).await;
            let mut transaction = fxfs
                .clone()
                .new_transaction(
                    lock_keys![LockKey::object(vol.store_object_id(), object_id)],
                    Options::default(),
                )
                .await
                .expect("new default encrypted file transaction");
            buffer.as_mut_slice()[..len].copy_from_slice(&inode.inline_data.as_ref().unwrap());
            object
                .raw_multi_write(
                    &mut transaction,
                    0,
                    Some(VOLUME_DATA_KEY_ID),
                    &[0..FXFS_BLOCK_SIZE as u64],
                    buffer.as_mut(),
                )
                .await
                .expect("write inline data");
            transaction.commit().await.expect("commit default encrypted file");
        } else {
            let verity_block_offset = if inode.header.advise_flags.contains(AdviseFlags::Verity) {
                Some(
                    (inode.header.size.next_multiple_of(64 * 1024) / F2FS_BLOCK_SIZE as u64) as u32,
                )
            } else {
                None
            };
            let mut buffer = object.allocate_buffer(FXFS_BLOCK_SIZE as usize).await;
            let mut blocks_iter = inode.data_blocks().peekable();
            while let Some((block_offset, block_addr)) = blocks_iter.next() {
                match verity_block_offset {
                    Some(verity_block_offset) if block_offset >= verity_block_offset => {
                        // The last block contains the fsverity descriptor.
                        if blocks_iter.peek().is_none() {
                            fxfs.device()
                                .read(
                                    offset + block_addr as u64 * F2FS_BLOCK_SIZE as u64,
                                    buffer.as_mut(),
                                )
                                .await
                                .expect("read f2fs data block");
                            let descriptor = FsVerityDescriptor::from_bytes(buffer.as_slice())?;
                            ensure!(
                                descriptor.file_size == inode.header.size,
                                "Verity file size mismatch"
                            );
                            // The whole data section should be finished. We can set verity here.
                            object.enable_verity(descriptor.fio_verity_options()).await?;
                        }
                        continue;
                    }
                    _ => {}
                };

                if block_addr == NULL_ADDR || block_addr == NEW_ADDR {
                    // Sparse block, skip. Fxfs handles sparse files.
                    continue;
                }
                fxfs.device()
                    .read(offset + block_addr as u64 * F2FS_BLOCK_SIZE as u64, buffer.as_mut())
                    .await
                    .expect("read f2fs data block");

                let mut transaction = fxfs
                    .clone()
                    .new_transaction(
                        lock_keys![LockKey::object(vol.store_object_id(), object_id)],
                        Options::default(),
                    )
                    .await
                    .expect("new default encrypted file transaction");
                object
                    .raw_multi_write(
                        &mut transaction,
                        DEFAULT_DATA_ATTRIBUTE_ID,
                        Some(VOLUME_DATA_KEY_ID),
                        &[block_offset as u64 * FXFS_BLOCK_SIZE as u64
                            ..(block_offset as u64 + 1) * FXFS_BLOCK_SIZE as u64],
                        buffer.as_mut(),
                    )
                    .await
                    .expect("write data block");
                transaction.commit().await.expect("commit default encrypted file");
            }
        }
    }
    Ok(())
}

/// Creates an Fxfs filesystem inside a device containing an f2fs filesystem using
/// free space, then rebuilds Fxfs metadata for the f2fs files such that they can be
/// read from Fxfs without requiring two copies of the data.
/// Note that once mounted in either format, the other filesystem will become invalid
/// Migrates an f2fs image to fxfs.
pub async fn migrate_device(
    offset: u64,
    device: DeviceHolder,
    crypt: Arc<dyn Crypt>,
) -> Result<DeviceHolder, Error> {
    // We shouldn't need to touch disk until the end.
    if !device.is_read_only() {
        device.reopen(/*read_only=*/ true);
    }

    let image_builder_mode =
        if offset > FXFS_BLOCK_SIZE as u64 { SuperBlockInstance::A } else { SuperBlockInstance::B };
    let mut fxfs = FxFilesystemBuilder::new()
        .format(true)
        .trim_config(None)
        .image_builder_mode(Some(image_builder_mode))
        .open(device)
        .await
        .context("Failed to open Fxfs")?;

    {
        let device = fxfs.device();
        let block_size = device.block_size() as u64;
        ensure!(offset % block_size == 0, "offset must be block aligned");
        let start_block = offset / block_size;
        let num_blocks = device.block_count() - start_block;

        let ranged_device = Arc::new(
            RangedDevice::new(device.clone(), start_block, num_blocks)
                .context("RangedDevice::new")?,
        );
        let f2fs =
            F2fsReader::open_device(ranged_device).await.context("Failed to open f2fs image")?;

        fxfs.journal().set_filesystem_uuid(&f2fs.superblock().uuid).expect("set uuid");

        // Create a "userdata" volume in fxfs.
        let root_volume = root_volume(fxfs.clone()).await.expect("Opening root volume");
        let vol = root_volume
            .new_volume(
                "userdata",
                NewChildStoreOptions {
                    options: StoreOptions { crypt: Some(crypt), ..StoreOptions::default() },
                    reserve_32bit_object_ids: true,
                    ..NewChildStoreOptions::default()
                },
            )
            .await
            .expect("Opening volume");
        let root_dir =
            Directory::open_unchecked(vol.clone(), vol.root_directory_object_id(), None, false);

        // Copy everything from f2fs to userdata, reusing existing extents.
        let mut files_to_copy = HashSet::new();
        let mut f2fs_metadata_blocks = HashSet::new();
        migrate(
            offset,
            &f2fs,
            &mut fxfs,
            f2fs.root_ino(),
            root_dir,
            &mut files_to_copy,
            &mut f2fs_metadata_blocks,
        )
        .await?;

        let metadata_object_handle;
        let mut transaction = fxfs
            .clone()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new reserve f2fs metadata transaction");
        metadata_object_handle = ObjectStore::create_object(
            &fxfs.root_store(),
            &mut transaction,
            HandleOptions::default(),
            None,
        )
        .await
        .expect("failed to create object");
        transaction.add(
            fxfs.root_store().store_object_id(),
            Mutation::replace_or_insert_object(
                ObjectKey::graveyard_entry(
                    fxfs.root_store().graveyard_directory_object_id(),
                    metadata_object_handle.object_id(),
                ),
                ObjectValue::Some,
            ),
        );

        reserve_f2fs_metadata(
            offset,
            &f2fs,
            f2fs.superblock().main_blkaddr,
            &f2fs_metadata_blocks,
            &files_to_copy,
            &mut transaction,
            &metadata_object_handle,
        )
        .await?;
        transaction.commit().await.context("commit txn")?;

        // multi_write mutates the disk -- reopen rw.
        fxfs.device().reopen(/*read_only=*/ false);

        deep_copy_files(offset, &f2fs, &mut fxfs, vol, files_to_copy).await?;

        // finalize() mutates disk, leave as rw.
        fxfs.finalize().await.expect("finalize");

        // TODO(https://fxbug.dev/439971580): Double check that after finalize, we still don't allow any old
        // extents to be deleted and we must not write to the super block.

        fxfs.close().await.expect("close fxfs");
    }
    let actual_size = fxfs.allocator().maximum_offset();
    let device = fxfs.take_device().await;
    println!("Final filesystem size is {actual_size}.");
    Ok(device)
}
