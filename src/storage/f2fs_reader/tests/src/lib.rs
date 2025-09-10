// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Context, Error, bail, ensure};
use f2fs_reader::{BLOCK_SIZE, F2fsReader, FileType, Flags, InlineFlags, Inode};
use fxfs::filesystem::{FxFilesystemBuilder, OpenFxFilesystem};
use fxfs::object_handle::{ObjectHandle, ObjectProperties};
use fxfs::object_store::journal::super_block::SuperBlockInstance;
use fxfs::object_store::transaction::{LockKey, Mutation, Options, lock_keys};
use fxfs::object_store::volume::root_volume;
use fxfs::object_store::{
    AttributeKey, DEFAULT_DATA_ATTRIBUTE_ID, Directory, EncryptionKey, ExtentValue, FSCRYPT_KEY_ID,
    HandleOptions, NO_OWNER, ObjectAttributes, ObjectDescriptor, ObjectKey, ObjectKind,
    ObjectStore, ObjectValue, PosixAttributes, Timestamp, VOLUME_DATA_KEY_ID,
};
use fxfs_crypto::Crypt;
use fxfs_insecure_crypto::InsecureCrypt;
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;
use storage_device::DeviceHolder;
use storage_device::fake_device::FakeDevice;

fn open_test_image(path: &str) -> FakeDevice {
    let path = std::path::PathBuf::from(path);
    FakeDevice::from_image(
        zstd::Decoder::new(std::fs::File::open(&path).expect("open image"))
            .expect("decompress image"),
        BLOCK_SIZE as u32,
    )
    .expect("open image")
}

fn inode_to_object_attributes(inode: &Inode, allocated_size: u64) -> ObjectAttributes {
    let mode = inode.header.mode;
    ObjectAttributes {
        creation_time: Timestamp { secs: inode.header.ctime, nanos: inode.header.ctime_nanos },
        modification_time: Timestamp { secs: inode.header.mtime, nanos: inode.header.mtime_nanos },
        project_id: 0,
        posix_attributes: Some(PosixAttributes {
            mode: mode.bits() as u32,
            uid: inode.header.uid,
            gid: inode.header.gid,
            rdev: 0,
        }),
        allocated_size,
        access_time: Timestamp { secs: inode.header.atime, nanos: inode.header.atime_nanos },
        change_time: Timestamp { secs: inode.header.ctime, nanos: inode.header.ctime_nanos },
    }
}

/// Helper to set the appropriate key type based on fscrypt context.
/// Returns (wrapping_key_id, key_id, keys)
async fn keys_from_context(
    object_id: u64,
    context: &Option<fscrypt::Context>,
    owner: &Directory<ObjectStore>,
    parent_is_fscrypt: bool,
    is_file: bool,
) -> Result<(Option<u128>, u64, Vec<(u64, EncryptionKey)>), Error> {
    if let Some(context) = context {
        ensure!(context.flags & fscrypt::POLICY_FLAGS_PAD_16 != 0, "require 16 byte padding");
        Ok((
            Some(0),        // Presence of wrapping_key_id implies fscrypt. Value irrelevant.
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
            if parent_is_fscrypt { Some(0) } else { None },
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
/// `existing_inodes` is used to handle hard links.
/// `f2fs_metadata_blocks` must be preserved to ensure that the resulting image is still parsable
/// as a valid f2fs image.
async fn migrate(
    f2fs: &F2fsReader,
    fxfs: &mut OpenFxFilesystem,
    ino: u32,
    dir: Directory<ObjectStore>,
    files_to_copy: &mut HashSet<u64>,
    f2fs_metadata_blocks: &mut Vec<u32>,
) -> Result<(), Error> {
    let mut existing_inodes = HashSet::new();

    let mut stack = vec![(ino, dir)];
    while let Some((ino, dir)) = stack.pop() {
        // Any dentry blocks for this directory are f2fs metadata.
        let inode = f2fs.read_inode(ino).await?;
        f2fs_metadata_blocks.extend_from_slice(&inode.block_addrs);
        f2fs_metadata_blocks.append(&mut inode.data_blocks().map(|(_, x)| x).collect());

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
            for xattr in &inode.xattr {
                // In f2fs, each xattr has an index byte that acts as a sort of namespace.
                // We will capture these verbatim and wire them into starnix.
                let mut name = vec![xattr.index as u8];
                name.extend_from_slice(&xattr.name);
                transaction.add(
                    dir.store().store_object_id(),
                    Mutation::replace_or_insert_object(
                        ObjectKey::extended_attribute(object_id, name),
                        ObjectValue::inline_extended_attribute(xattr.value.to_vec()),
                    ),
                );
            }

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
                    f2fs_metadata_blocks.extend_from_slice(&inode.block_addrs);

                    let mut allocated_size = 0;
                    let inline_flags = inode.header.inline_flags;
                    //let mut mutation_count = 4;
                    if inline_flags.contains(InlineFlags::Data) {
                        if inode.header.size > 0 {
                            // We have to allocate inline files.
                            // Encrypted inline files are not possible, so this is relatively uncommon.
                            files_to_copy.insert(object_id);
                            allocated_size = BLOCK_SIZE as u64;
                        }
                    } else if inode.context.is_some() {
                        // Fscrypt file, extents are remapped.
                        for (block_offset, block_addr) in inode.data_blocks() {
                            let device_range = block_addr as u64 * BLOCK_SIZE as u64
                                ..(block_addr as u64 + 1) * BLOCK_SIZE as u64;
                            let logical_range = block_offset as u64 * BLOCK_SIZE as u64
                                ..(block_offset as u64 + 1) * BLOCK_SIZE as u64;
                            dir.store()
                                .mark_allocated(
                                    &mut transaction,
                                    dir.store().store_object_id(),
                                    device_range.clone(),
                                )
                                .await?;
                            transaction.add(
                                dir.store().store_object_id(),
                                Mutation::merge_object(
                                    ObjectKey::extent(
                                        object_id,
                                        DEFAULT_DATA_ATTRIBUTE_ID,
                                        logical_range,
                                    ),
                                    ObjectValue::Extent(ExtentValue::new_raw(
                                        device_range.start,
                                        key_id,
                                    )),
                                ),
                            );
                            allocated_size += BLOCK_SIZE as u64;
                        }
                    } else {
                        // Default encrypted file, data will be copied later.
                        files_to_copy.insert(object_id);
                        // Max blocks per file is ~1B and block count is stored in a u32 so
                        // there is basically no chance of this overflowing.
                        allocated_size = inode.data_blocks().count() as u64 * BLOCK_SIZE as u64;
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
                    f2fs_metadata_blocks.extend_from_slice(&inode.block_addrs);

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
                _ => unimplemented!(),
            }
        }
    }
    Ok(())
}

async fn verify(
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
                wrapping_key_id = Some(0);
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
                        let mut name = vec![xattr.index as u8];
                        name.extend_from_slice(&xattr.name);
                        let fxfs_xattr_value =
                            dir.get_extended_attribute(name).await.context("xattr read")?;
                        assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
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
                        let mut name = vec![xattr.index as u8];
                        name.extend_from_slice(&xattr.name);
                        let fxfs_xattr_value =
                            handle.get_extended_attribute(name).await.context("xattr read")?;
                        assert_eq!(&fxfs_xattr_value, xattr.value.as_ref());
                    }

                    let fxfs_properties =
                        handle.get_properties().await.context("get properties")?;
                    let f2fs_allocated_size = if let Some(data) = inode.inline_data.as_ref() {
                        if data.len() > 0 { BLOCK_SIZE as u64 } else { 0 }
                    } else {
                        inode.data_blocks().count() as u64 * BLOCK_SIZE as u64
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
                    assert_eq!(fxfs_properties, f2fs_properties);

                    if check_file_contents {
                        let inline_flags = inode.header.inline_flags;
                        if inline_flags.contains(InlineFlags::Data) {
                            let mut buffer = handle.allocate_buffer(BLOCK_SIZE).await;
                            let len = handle.read(0, 0, buffer.as_mut()).await.context("read")?;
                            let f2fs_block = inode.inline_data.as_ref().unwrap();
                            assert_eq!(
                                &buffer.as_slice()[..len],
                                f2fs_block.as_ref(),
                                "Inline data mismatch."
                            );
                        } else {
                            let device = fxfs.device();
                            let mut fxfs_buffer = device.allocate_buffer(BLOCK_SIZE).await;
                            for i in 0..inode.header.block_size as u32 {
                                if let Some(f2fs_block) = f2fs.read_data(&inode, i).await.unwrap() {
                                    let len = handle
                                        .read(0, i as u64 * BLOCK_SIZE as u64, fxfs_buffer.as_mut())
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
async fn reserve_f2fs_metadata(
    f2fs: &F2fsReader,
    fxfs: &mut OpenFxFilesystem,
    f2fs_main_blkaddr: u32, // Start of the 'data' region.
    blocks: &[u32],
    files_to_copy: &HashSet<u64>,
) -> Result<(), Error> {
    let handle;
    let mut transaction = fxfs
        .clone()
        .new_transaction(lock_keys![], Options::default())
        .await
        .expect("new reserve f2fs metadata transaction");
    handle = ObjectStore::create_object(
        &fxfs.root_store(),
        &mut transaction,
        HandleOptions::default(),
        None,
    )
    .await
    .expect("failed to create object");
    // Region between first and second fxfs superblock.
    handle.extend(&mut transaction, 4096..128 * BLOCK_SIZE as u64).await.context("extend a")?;
    // Region after second fxfs superblock to end of f2fs metadata region.
    handle
        .extend(
            &mut transaction,
            129 * BLOCK_SIZE as u64..f2fs_main_blkaddr as u64 * BLOCK_SIZE as u64,
        )
        .await
        .context("extend b")?;
    for &block in blocks {
        let byte_range = block as u64 * BLOCK_SIZE as u64..(block as u64 + 1) * BLOCK_SIZE as u64;
        handle.extend(&mut transaction, byte_range).await.context("extend c")?;
    }
    transaction.add(
        fxfs.root_store().store_object_id(),
        Mutation::replace_or_insert_object(
            ObjectKey::graveyard_entry(
                fxfs.root_store().graveyard_directory_object_id(),
                handle.object_id(),
            ),
            ObjectValue::Some,
        ),
    );
    transaction.commit().await.context("commit txn")?;

    // We must add the files we intend to copy to the set of reserved data or else
    // we might end up overwriting one of them while copying another.
    let mut transaction = fxfs
        .clone()
        .new_transaction(
            lock_keys![LockKey::object(handle.store().store_object_id(), handle.object_id())],
            Options::default(),
        )
        .await
        .expect("new reserve f2fs metadata transaction");
    // TODO(b/394701234): We need to ensure that this works with a lot of files and very large
    // files.
    for ino in files_to_copy {
        let inode = f2fs.read_inode(*ino as u32).await?;
        for (_block_offset, block_addr) in inode.data_blocks() {
            let byte_range =
                block_addr as u64 * BLOCK_SIZE as u64..(block_addr as u64 + 1) * BLOCK_SIZE as u64;
            handle.extend(&mut transaction, byte_range).await.context("extend d")?;
        }
    }
    transaction.commit().await.context("commit txn")?;
    Ok(())
}

/// Creates an Fxfs filesystem inside a device containing an f2fs filesystem using
/// free space, then rebuilds Fxfs metadata for the f2fs files such that they can be
/// read from Fxfs without requiring two copies of the data.
/// Note that once mounted in either format, the other filesystem will become invalid
/// and should not be used.
async fn migrate_device(
    device: DeviceHolder,
    crypt: &Arc<dyn Crypt>,
) -> Result<DeviceHolder, Error> {
    // We shouldn't need to touch disk until the end.
    device.reopen(/*read_only=*/ true);

    let mut fxfs = FxFilesystemBuilder::new()
        .format(true)
        .trim_config(None)
        // F2fs superblock is stored in same block as Fxfs block A, so avoid that.
        .image_builder_mode(Some(SuperBlockInstance::B))
        .open(device)
        .await
        .expect("Failed to create fxfs filesystem builder");

    {
        let f2fs = Box::new(F2fsReader::open_device(fxfs.device()).await.expect("f2fs open ok"));

        fxfs.journal().set_filesystem_uuid(&f2fs.superblock.uuid).expect("set uuid");

        // Create a "userdata" volume in fxfs.
        let root_volume = root_volume(fxfs.clone()).await.expect("Opening root volume");
        let vol = root_volume
            .new_volume("userdata", NO_OWNER, Some(crypt.clone()))
            .await
            .expect("Opening volume");
        let root_directory =
            Directory::open_unchecked(vol.clone(), vol.root_directory_object_id(), None, false);

        // Copy everything from f2fs to userdata, reusing existing extents.
        let ino = f2fs.root_ino();
        let mut files_to_copy = HashSet::new();
        let mut f2fs_metadata_blocks = Vec::new();
        migrate(
            &f2fs,
            &mut fxfs,
            ino,
            root_directory,
            &mut files_to_copy,
            &mut f2fs_metadata_blocks,
        )
        .await
        .expect("walk");

        // TODO(b/393448875): We are using the graveyard here to reserve the extents containing f2fs
        // metadata until next boot. This could be avoided with a bit more work. Currently unclear
        // if this is worth the complexity though.
        //
        // The Fxfs allocator caps the number of free extents it holds in its free lists in RAM.
        // If it exhausts its memory-backed free lists, it will scan the allocator LSM tree to
        // find more extents. In this case we're reaching in and manipulating the in-memory
        // structure without associated LSM tree commitments so, while unlikely, there is a risk
        // that in very large filesystems we might run into this allocator 'rebuild' behavior.
        reserve_f2fs_metadata(
            &f2fs,
            &mut fxfs,
            f2fs.superblock.main_blkaddr,
            &f2fs_metadata_blocks,
            &files_to_copy,
        )
        .await
        .expect("reserve f2fs metadata");

        // Bump last_object_id to avoid an inode collision with data we just added.
        vol.maybe_bump_last_object_id(f2fs.max_ino() as u64).expect("bump last_object_id");

        // multi_write mutates the disk -- reopen rw.
        fxfs.device().reopen(/*read_only=*/ false);

        for object_id in files_to_copy {
            let inode = f2fs.read_inode(object_id as u32).await?;
            let object = ObjectStore::open_object(
                &vol,
                object_id,
                HandleOptions::default(),
                Some(crypt.clone()),
            )
            .await?;
            if inode.header.inline_flags.contains(InlineFlags::Data) {
                let len = inode.inline_data.as_ref().unwrap().len();
                let mut buffer = object.allocate_buffer(BLOCK_SIZE).await;
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
                        &[0..BLOCK_SIZE as u64],
                        buffer.as_mut(),
                    )
                    .await
                    .expect("write inline data");
                transaction.commit().await.expect("commit default encrypted file");
            } else {
                for (block_offset, block_addr) in inode.data_blocks() {
                    let mut buffer = object.allocate_buffer(BLOCK_SIZE).await;
                    fxfs.device()
                        .read(block_addr as u64 * BLOCK_SIZE as u64, buffer.as_mut())
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
                            0,
                            Some(VOLUME_DATA_KEY_ID),
                            &[block_offset as u64 * BLOCK_SIZE as u64
                                ..(block_offset as u64 + 1) * BLOCK_SIZE as u64],
                            buffer.as_mut(),
                        )
                        .await
                        .expect("write data block");
                    transaction.commit().await.expect("commit default encrypted file");
                }
            }
        }

        // finalize() mutates disk, leave as rw.
        fxfs.finalize().await.expect("finalize");

        // TODO(b/439971580): Double check that after finalize, we still don't allow any old
        // extents to be deleted and we must not write to the super block.

        fxfs.close().await.expect("close fxfs");
    }
    let actual_size = fxfs.allocator().maximum_offset();
    let device = fxfs.take_device().await;
    println!("Final filesystem size is {actual_size}.");
    Ok(device)
}

// Migrates an f2fs device to fxfs and verifies directory tree matches.
// Note this test can't verify file contents as we haven't given encryption keys.
#[fuchsia::test]
async fn test_fxfs_migration_no_keys() {
    let device = DeviceHolder::new(open_test_image("/pkg/testdata/f2fs.img.zst"));
    let f2fs = F2fsReader::open_device(device.deref().clone()).await.expect("f2fs open ok");
    let original_superblock = f2fs.superblock;
    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.add_wrapping_key(0, [0; 64].into());
    insecure_crypt.set_filesystem_uuid(&f2fs.superblock.uuid);
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);
    let device = migrate_device(device, &crypt).await.unwrap();

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");
    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.set_filesystem_uuid(&f2fs.superblock.uuid);
    let crypt: Option<Arc<dyn Crypt>> = Some(Arc::new(insecure_crypt));

    // Re-open as f2fs and do it all again, this time verifying.
    let f2fs = F2fsReader::open_device(fxfs.device().clone()).await.expect("f2fs open ok");
    assert_eq!(original_superblock, f2fs.superblock);

    fxfs::fsck::fsck(fxfs.clone()).await.expect("fsck failed");
    let root_volume = root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol =
        root_volume.volume("userdata", NO_OWNER, crypt.clone()).await.expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), crypt).await.expect("fsck volume");
    let root_directory =
        Directory::open(&vol, vol.root_directory_object_id()).await.expect("open failed");
    let ino = f2fs.root_ino();

    // Note that we can't check file contents in this test as we haven't given fxfs encryption keys.
    let check_file_contents = false;
    verify(&f2fs, &fxfs, ino, root_directory, check_file_contents).await.expect("verify");

    fxfs.close().await.expect("close ok");
}

async fn recurse_resolve_f2fs(f2fs: &F2fsReader, ino: u32, path: &str) -> u32 {
    if let Some((head, rest)) = path.split_once("/") {
        for entry in f2fs.readdir(ino).await.expect("readdir") {
            if entry.filename == head {
                return Box::pin(recurse_resolve_f2fs(f2fs, entry.ino, rest)).await;
            }
        }
    } else {
        for entry in f2fs.readdir(ino).await.expect("readdir") {
            if entry.filename == path {
                return entry.ino;
            }
        }
    }
    panic!("Path not found: {path:?}");
}

// Read a single file encrypted with fscrypt's INO_LBLK32 mode.
#[fuchsia::test]
async fn test_fxfs_read_lblk32_ino_file() {
    let device = DeviceHolder::new(open_test_image("/pkg/testdata/f2fs.img.zst"));
    let mut f2fs = F2fsReader::open_device(device.deref().clone()).await.expect("f2fs open ok");
    f2fs.add_key(&[0; 64]);

    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.set_filesystem_uuid(&f2fs.superblock.uuid);
    insecure_crypt.add_wrapping_key(
        u128::from_le_bytes(fscrypt::main_key_to_identifier(&[0; 64])),
        [0; 64].into(),
    );
    insecure_crypt.add_wrapping_key(0, [0; 64].into());
    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);

    let device = migrate_device(device, &crypt).await.unwrap();

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");

    let root_volume = root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol = root_volume
        .volume("userdata", NO_OWNER, Some(crypt.clone()))
        .await
        .expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), Some(crypt.clone()))
        .await
        .expect("fsck volume");

    // Inode numbers should remain the same across migration, so we can lookup in f2fs and jump to
    // the inode in fxfs (i.e. we are testing file encryption without directory parsing).
    let ino = recurse_resolve_f2fs(&f2fs, f2fs.root_ino(), "fscrypt/a/b/inlined").await;
    let inode = f2fs.read_inode(ino).await.expect("read file");
    let f2fs_data = f2fs.read_data(&inode, 0).await.expect("read data");

    // This is the data originally written into the file via our generation script.
    const EXPECTED_CONTENTS: &[u8] = b"test45678abcdef_12345678";
    // Confirm f2fs returns this data.
    assert_eq!(
        &f2fs_data.as_ref().unwrap().as_slice()[..EXPECTED_CONTENTS.len()],
        EXPECTED_CONTENTS
    );

    // Confirm fxfs also returns this data.
    let fxfs_object = ObjectStore::open_object(&vol, ino as u64, HandleOptions::default(), None)
        .await
        .expect("open object");
    let mut buf = fxfs_object.allocate_buffer(4096).await;
    assert_eq!(fxfs_object.read(0, 0, buf.as_mut()).await.expect("read"), EXPECTED_CONTENTS.len());
    assert_eq!(&buf.as_slice()[..EXPECTED_CONTENTS.len()], EXPECTED_CONTENTS);

    fxfs.close().await.expect("close ok");
}

#[fuchsia::test]
async fn test_fxfs_verify_encrypted_data() {
    let device = DeviceHolder::new(open_test_image("/pkg/testdata/f2fs.img.zst"));
    let f2fs = F2fsReader::open_device(device.deref().clone()).await.expect("f2fs open ok");

    let mut insecure_crypt = InsecureCrypt::new();
    insecure_crypt.set_filesystem_uuid(&f2fs.superblock.uuid);
    insecure_crypt.add_wrapping_key(
        u128::from_le_bytes(fscrypt::main_key_to_identifier(&[0; 64])),
        [0; 64].into(),
    );

    let crypt: Arc<dyn Crypt> = Arc::new(insecure_crypt);
    let device = migrate_device(device, &crypt).await.unwrap();

    // Reopen RW so we can mount Fxfs normally.
    device.reopen(false);
    let fxfs = FxFilesystemBuilder::new().read_only(true).open(device).await.expect("open failed");

    // Re-open as f2fs and read encrypted files from both filesystems.
    let mut f2fs = F2fsReader::open_device(fxfs.device().clone()).await.expect("f2fs open ok");
    assert_eq!(&f2fs.superblock.uuid, fxfs.super_block_header().guid.0.as_bytes());
    f2fs.add_key(&[0; 64]);

    let root_volume = root_volume(fxfs.clone()).await.expect("Opening root volume");
    let vol = root_volume
        .volume("userdata", NO_OWNER, Some(crypt.clone()))
        .await
        .expect("Opening volume");
    fxfs::fsck::fsck_volume(&fxfs, vol.store_object_id(), Some(crypt.clone()))
        .await
        .expect("fsck volume");
    let root_directory =
        Directory::open(&vol, vol.root_directory_object_id()).await.expect("open failed");
    let ino = f2fs.root_ino();
    verify(&f2fs, &fxfs, ino, root_directory, /*check_file_contents=*/ true).await.expect("verify");
    fxfs.close().await.expect("close ok");
}
