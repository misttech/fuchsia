// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Allows installation of an fxfs volume contained by a file handle into its owning volume.
//!
//! *WARNING*: This is still under active development and is not yet ready for production use.
//! Use with caution.

// TODO(https://fxbug.dev/415300916): Make installation paths non-pub so that they can only be
// triggered from within this sub-module.

use crate::errors::FxfsError;
use crate::filesystem::FxFilesystemBuilder;
use crate::lsm_tree::persistent_layer::PersistentLayerWriter;
use crate::lsm_tree::skip_list_layer::SkipListLayer;
use crate::lsm_tree::types::{Item, ItemRef, Layer, LayerIterator, LayerWriter as _};
use crate::lsm_tree::{LayerSet, Query, layers_from_handles};
use crate::object_store::extent_mapping_iterator::ExtentMappingIterator;
use crate::object_store::extent_record::{ExtentKey, ExtentMode, ExtentValue};
use crate::object_store::object_manager::ReservationUpdate;
use crate::object_store::object_record::{
    AttributeKey, ObjectAttributes, ObjectKey, ObjectKeyData, ObjectKind, ObjectValue,
};
use crate::object_store::transaction::{AssocObj, LockKey, Mutation, Options, lock_keys};
use crate::object_store::tree::reservation_amount_from_layer_size;
use crate::object_store::volume::root_volume;
use crate::object_store::{
    DataObjectHandle, DirectWriter, FileExtent, HandleOptions, HandleOwner, INVALID_OBJECT_ID,
    ObjectHandle, ObjectStore, ReadObjectHandle as _, StoreInfo, merge,
};
use crate::range::RangeExt;
use crate::virtual_device::ReadOnlyDevice;
use anyhow::{Context, Error, bail};
use std::sync::Arc;
use storage_device::DeviceHolder;

use crate::object_store::NO_OWNER;

impl ObjectStore {
    /// Installs an inner volume. The inner volume must exist as a volume within a filesystem stored
    /// contiguously within `handle`. `handle` must exist within this volume. There can be no other
    /// objects in the outer volume with any extent records, and neither volume can be encrypted.
    // TODO(https://fxbug.dev/415300916): Transition this such that the ObjectManager starts the
    // installation process upon the next mount. This should give us greater guarantees that the
    // state of the volume is what we expect, and there are no stale handles to the inner file.
    pub async fn install_inner_volume(
        &self,
        handle: DataObjectHandle<ObjectStore>,
        name: &str,
    ) -> Result<(), Error> {
        // *NOTE*: If other than `handle` *any* other objects in this volume contain extent records,
        // the installation process will leave the filesystem with orphaned allocations! To guard
        // against this, we ensure only the object ID from `handle` has any extent records.
        if !item_is_unique(self, &handle).await? {
            return Err(FxfsError::Internal)
                .context("Cannot install volume: outer volume contains extra extent records");
        }

        // Extract the extents from the backing handle, and then mount the inner filesystem.
        let extents = handle.device_extents().await?;
        // TODO(https://fxbug.dev/415300916): We are currently assuming the block size of the image
        // matches the filesystem as we have no way of verifying this. We should add a field to the
        // superblock to guard against this, and also prevent mounting an fxfs partition on a device
        // with the wrong block size.
        let device = DeviceHolder::new(ReadOnlyDevice::new(handle)?);
        let inner_fs = FxFilesystemBuilder::new().read_only(true).open(device).await?;

        {
            let root_volume = root_volume(inner_fs.clone()).await?;
            let inner_store = root_volume
                .volume(name, NO_OWNER, None)
                .await
                .context("unable to open inner volume {name}")?;
            self.install_inner_volume_impl(inner_store, extents).await?;
        }

        // *NOTE*: At this point, `handle` is completely invalidated as it no longer matches the
        // on-disk state of this store. As a precaution, we make sure there are no outstanding
        // references to the device that owns the handle.
        let _ = inner_fs.take_device().await;
        Ok(())
    }

    /// Installs a new inner volume by performing the following steps:
    ///
    ///   1. We create a new object in-memory that references the portions of the inner filesystem
    ///      that we wish to discard when the volume has been installed.
    ///   2. We create a new object in the parent store holding the desired object tree layer once
    ///      the volume is installed. This includes the objects currently inside the inner volume
    ///      as well as the additional file that owns the metadata allocations created in step 1.
    ///   3. We write out the new object tree layer, but remapping the extents of the inner
    ///      filesystem to match those of the storage backing it.
    ///   4. Update the on-disk and in-memory state of the object store to be consistent with the
    ///      new object tree layer.
    ///   5. Delete (tombstone) the unused metadata extents and the old layer files.
    async fn install_inner_volume_impl(
        &self,
        inner_store: Arc<ObjectStore>,
        extents: Vec<FileExtent>,
    ) -> Result<(), Error> {
        let fs = self.filesystem();
        let txn_guard = fs.clone().txn_guard().await;
        let keys = lock_keys![LockKey::flush(self.store_object_id())];
        let _guard = Some(fs.lock_manager().write_lock(keys).await);

        // Step 1: Create a new object that owns the portions of the backing file that we want to
        // discard once the volume has been installed.
        let metadata_object_id = inner_store.maybe_get_next_object_id();
        assert!(metadata_object_id != INVALID_OBJECT_ID);
        let mut inner_layer_set = inner_store.tree().layer_set();
        let layer_with_metadata =
            create_metadata_ownership_layer(&inner_layer_set, metadata_object_id, &extents).await?;
        // Add the file to the graveyard so it's cleaned up if we crash.
        layer_with_metadata.insert(Item {
            key: ObjectKey::graveyard_entry(
                inner_store.graveyard_directory_object_id(),
                metadata_object_id,
            ),
            value: ObjectValue::Some,
            sequence: 0,
        })?;
        let locked = (layer_with_metadata as Arc<dyn Layer<_, _>>).into();
        inner_layer_set.layers.push(locked);

        let object_manager = fs.object_manager();
        let reservation = object_manager.metadata_reservation();
        let txn_options = Options {
            skip_journal_checks: true,
            borrow_metadata_space: true,
            allocator_reservation: Some(reservation),
            txn_guard: Some(&txn_guard),
            ..Default::default()
        };

        let mut transaction = fs.clone().new_transaction(lock_keys![], txn_options).await?;
        transaction.add(self.store_object_id(), Mutation::BeginFlush);
        transaction.commit().await?;

        // Step 2: Create an object for the new layer we will create for the installed volume.
        // For this, we need two transactions: one to create the new layer (graveyarded so it's
        // cleaned up if we crash), and another that atomically swaps in the new layer and
        // graveyards the old ones.
        let mut create_new_layer_txn =
            fs.clone().new_transaction(lock_keys![], txn_options).await?;
        let reservation_update: ReservationUpdate; // Must outlive `activate_volume_txn`.
        let parent_store = self.parent_store.as_ref().unwrap();

        let mut activate_volume_txn = fs
            .clone()
            .new_transaction(
                lock_keys![LockKey::object(
                    parent_store.store_object_id(),
                    self.store_info_handle_object_id().unwrap(),
                )],
                txn_options,
            )
            .await?;

        let new_layer = {
            let handle_options = HandleOptions { skip_journal_checks: true, ..Default::default() };
            let new_layer = ObjectStore::create_object(
                parent_store,
                &mut create_new_layer_txn,
                handle_options,
                None,
            )
            .await?;
            parent_store.add_to_graveyard(&mut create_new_layer_txn, new_layer.object_id());
            parent_store.remove_from_graveyard(&mut activate_volume_txn, new_layer.object_id());
            create_new_layer_txn.commit().await?;
            new_layer
        };

        // Step 3: Write out the new object tree layer but with remapped extent records.
        {
            let writer = DirectWriter::new(&new_layer, txn_options).await;
            // TODO(https://fxbug.dev/415300916): Our extent mapping iterator may emit more items
            // than are in the current layer set so `num_items` will likely be too small here. We
            // should calculate the correct amount ahead of time before emitting the object records.
            let num_items = inner_layer_set.sum_len();
            let mut writer = PersistentLayerWriter::new(writer, num_items, fs.block_size()).await?;
            let mut merger = inner_layer_set.merger();
            let mut iter =
                ExtentMappingIterator::new(merger.query(Query::FullScan).await?, extents)?;
            while let Some(item_ref) = iter.get() {
                writer.write(item_ref).await?;
                iter.advance().await?;
            }
            writer.flush().await?;
        }

        // Move the old layers to the graveyard at the end.
        let old_layers = self.tree.immutable_layer_set().layers;
        for layer in &old_layers {
            if let Some(handle) = layer.handle() {
                parent_store.add_to_graveyard(&mut activate_volume_txn, handle.object_id());
            }
        }

        let total_layer_size = new_layer.get_size();
        let new_layer_object_id = new_layer.object_id();
        let new_layers = layers_from_handles([new_layer]).await?;

        let old_store_info = inner_store.store_info().unwrap();
        let new_store_info = StoreInfo {
            layers: vec![new_layer_object_id],
            object_count: old_store_info.object_count + 1, // Update for the metadata file we added
            ..old_store_info
        };

        // Step 4: Update the on-disk and in-memory state of the object store.
        self.write_store_info(&mut activate_volume_txn, &new_store_info).await?;
        reservation_update =
            ReservationUpdate::new(reservation_amount_from_layer_size(total_layer_size));
        activate_volume_txn.add_with_object(
            self.store_object_id(),
            Mutation::EndFlush,
            AssocObj::Borrowed(&reservation_update),
        );
        activate_volume_txn
            .commit_with_callback(|_| {
                *self.store_info.lock().as_mut().unwrap() = new_store_info;
                self.tree.set_layers(new_layers);
            })
            .await?;

        // Step 5: Delete (tombstone) the old layer files and the file containing unused metadata.
        for layer in old_layers {
            let object_id = layer.handle().map(|h| h.object_id());
            layer.close_layer().await;
            if let Some(object_id) = object_id {
                parent_store.tombstone_object(object_id, txn_options).await?;
            }
        }
        self.tombstone_object(metadata_object_id, txn_options).await?;

        Ok(())
    }
}

/// Creates an in-memory layer containing a single file which owns the extents that are NOT
/// referenced by `inner_layer_set`. If this file is deleted, the extent records referenced by
/// `inner_layer_set` will still exist on disk. However, the remaining allocations for the volume,
/// including the current layers for `inner_layer_set`, will be deleted. This is only safe to do so
/// after the new volume has been installed (e.g. the object records have been remapped).
async fn create_metadata_ownership_layer(
    inner_layer_set: &LayerSet<ObjectKey, ObjectValue>,
    object_id: u64,
    extents: &[FileExtent],
) -> Result<Arc<SkipListLayer<ObjectKey, ObjectValue>>, Error> {
    if extents.is_empty() {
        return Err(FxfsError::Inconsistent).context("store has no backing extents");
    }
    // Create an in in-memory layer containing a file to be deleted that contains all the other
    // extents we don't need.
    // TODO(https://fxbug.dev/415300916): Calculate the exact amount of items we need.
    let layer = SkipListLayer::new(1000);

    // NOTE: Backing extents are assumed to be sorted by logical offset, so the last extent's end
    // offset should be equal to the size of the backing file.
    let mut size = extents.last().unwrap().logical_range().end;
    layer.insert(Item {
        key: ObjectKey::extent(object_id, 0, 0..size),
        value: ObjectValue::Extent(ExtentValue::Some {
            device_offset: 0, // Will be remapped below
            mode: ExtentMode::Raw,
            key_id: 0,
        }),
        sequence: 0,
    })?;

    // Iterate over all the extent records in the layer set, and punch holes in the new file we
    // created for each record.
    {
        let mut merger = inner_layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        while let Some(item_ref) = iter.get() {
            if let ObjectValue::Extent(ExtentValue::Some { device_offset, .. }) = item_ref.value {
                // NOTE: `device_offset` refers to the logical offset within the backing file.
                // This is equivalent to the *logical* offset within `extents`. Thus we need to use
                // the device range of the extent records as the logical range we want to remove.
                let ObjectKey {
                    data: ObjectKeyData::Attribute(_, AttributeKey::Extent(ExtentKey { range })),
                    ..
                } = item_ref.key
                else {
                    bail!(FxfsError::Inconsistent);
                };
                let len = range.length()?;
                let item = Item {
                    key: ObjectKey::extent(object_id, 0, *device_offset..*device_offset + len),
                    value: ObjectValue::Extent(ExtentValue::None),
                    sequence: 0,
                };
                size = size
                    .checked_sub(len)
                    .ok_or(FxfsError::Inconsistent)
                    .context("blob extents are larger than written image")?;
                let lower_bound = item.key.key_for_merge_into();
                layer.merge_into(item, &lower_bound, merge::merge);
            }
            iter.advance().await?;
        }
    }

    // Add the extra object records we need for it to be a valid file.
    layer.insert(Item {
        key: ObjectKey::object(object_id),
        value: ObjectValue::Object {
            kind: ObjectKind::File { refs: 1 },
            attributes: ObjectAttributes { allocated_size: size, ..Default::default() },
        },
        sequence: 0,
    })?;

    layer.insert(Item {
        key: ObjectKey::attribute(object_id, 0, AttributeKey::Attribute),
        value: ObjectValue::Attribute { size, has_overwrite_extents: false },
        sequence: 0,
    })?;

    Ok(layer)
}

/// Returns true if all extent records within `owner` are those referenced by `item`.
async fn item_is_unique<H: HandleOwner>(
    owner: &H,
    handle: &DataObjectHandle<H>,
) -> Result<bool, Error> {
    let layer_set = owner.as_ref().tree().layer_set();
    let mut merger = layer_set.merger();
    let mut iter = merger.query(Query::FullScan).await?;
    while let Some(item) = iter.get() {
        if let ItemRef {
            key:
                ObjectKey {
                    object_id,
                    data: ObjectKeyData::Attribute(attribute_id, AttributeKey::Extent(_)),
                },
            value: ObjectValue::Extent(_),
            sequence: _,
        } = item
        {
            if *object_id != handle.object_id() || *attribute_id != handle.attribute_id() {
                return Ok(false);
            }
        }
        iter.advance().await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::{FxFilesystem, SyncOptions};
    use crate::fsck::{FsckOptions, fsck_with_options};
    use crate::object_handle::WriteObjectHandle as _;
    use crate::object_store::directory::Directory;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    const INNER_VOLUME_NAME: &str = "inner";

    /// Creates an in-memory fxfs filesystem with the given contents.
    async fn create_test_filesystem(
        block_count: u64,
        block_size: u32,
        volume_name: &str,
        files: &[(&str, &str)],
        read_only: bool,
    ) -> DeviceHolder {
        let device = DeviceHolder::new(FakeDevice::new(block_count, block_size));
        let fs = FxFilesystem::new_empty(device).await.unwrap();
        {
            let root = root_volume(fs.clone()).await.unwrap();
            let volume = root.new_volume(volume_name, NO_OWNER, None).await.unwrap();
            let inner_dir =
                Directory::open(&volume, volume.root_directory_object_id()).await.unwrap();
            // Let's create some files that contain some data we'll read back.
            let mut handles = vec![];
            let keys = lock_keys![LockKey::object(volume.store_object_id(), inner_dir.object_id())];
            let mut transaction = volume
                .filesystem()
                .clone()
                .new_transaction(keys, Default::default())
                .await
                .unwrap();
            for (name, contents) in files.iter() {
                let handle = inner_dir.create_child_file(&mut transaction, *name).await.unwrap();
                handles.push((handle, *contents));
            }
            transaction.commit().await.unwrap();
            {
                let device = fs.device();
                for (handle, contents) in handles {
                    let mut buf = device.allocate_buffer(contents.len()).await;
                    buf.as_mut_slice()[0..contents.len()].copy_from_slice(contents.as_bytes());
                    handle.write_or_append(None, buf.as_ref()).await.unwrap();
                }
            }
        }
        fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.unwrap();
        fs.close().await.unwrap();
        let device = fs.take_device().await;
        device.reopen(read_only);
        device
    }

    async fn write_image_to_handle(image: &DeviceHolder, handle: &DataObjectHandle<ObjectStore>) {
        const CHUNK_READ_SIZE: usize = 131_072; /* 128 KiB */
        let mut inner_buff = image.allocate_buffer(CHUNK_READ_SIZE).await;
        let outer_device = handle.owner().filesystem().device();
        let mut outer_buff = outer_device.allocate_buffer(CHUNK_READ_SIZE).await;
        let total = image.size();
        let mut offset = 0;
        while offset < total {
            let amount = std::cmp::min(total - offset, CHUNK_READ_SIZE as u64);
            image.read(offset, inner_buff.as_mut()).await.unwrap();
            outer_buff.as_mut_slice().copy_from_slice(inner_buff.as_slice());
            handle
                .write_or_append(Some(offset), outer_buff.subslice(0..amount as usize))
                .await
                .unwrap();
            offset += amount;
        }
        assert_eq!(offset, total);
        handle.flush().await.unwrap();
    }

    #[fuchsia::test]
    async fn test_install_volume() {
        let inner_files = [
            ("file_1", "Hello, world!"),
            ("file_2", "Goodbye, stranger!"),
            ("file_3", "Its been nice..."),
        ];
        let outer_fs =
            FxFilesystem::new_empty(DeviceHolder::new(FakeDevice::new(1024, 4096))).await.unwrap();

        // Install a new volume on top of "vol" containing the above files.
        {
            let outer_root = root_volume(outer_fs.clone()).await.unwrap();
            let outer_volume = outer_root.new_volume("vol", NO_OWNER, None).await.unwrap();
            let outer_dir = Directory::open(&outer_volume, outer_volume.root_directory_object_id())
                .await
                .unwrap();

            let inner_fs =
                create_test_filesystem(512, 4096, INNER_VOLUME_NAME, &inner_files, true).await;

            let image_handle = {
                let keys: crate::object_store::transaction::LockKeys = lock_keys![LockKey::object(
                    outer_volume.store_object_id(),
                    outer_dir.object_id()
                )];
                let mut transaction = outer_volume
                    .filesystem()
                    .clone()
                    .new_transaction(keys, Default::default())
                    .await
                    .unwrap();
                let handle = outer_dir
                    .create_child_file(&mut transaction, "volume_to_install")
                    .await
                    .unwrap();
                transaction.commit().await.unwrap();
                handle
            };

            write_image_to_handle(&inner_fs, &image_handle).await;
            outer_volume.install_inner_volume(image_handle, INNER_VOLUME_NAME).await.unwrap();
            outer_fs.sync(SyncOptions { flush_device: true, ..Default::default() }).await.unwrap();
        };
        outer_fs.close().await.unwrap();
        let device = outer_fs.take_device().await;

        // Reopen the filesystem and verify that we installed the contents of the inner volume into
        // the outer volume.
        device.reopen(false);
        let outer_fs = FxFilesystem::open(device).await.unwrap();
        let fsck_options = FsckOptions {
            fail_on_warning: true,
            on_error: Box::new(|err| eprintln!("fsck error: {:?}", err)),
            ..Default::default()
        };
        fsck_with_options(outer_fs.clone(), &fsck_options).await.expect("fsck failed");

        let outer_root = root_volume(outer_fs.clone()).await.unwrap();
        let installed_volume = outer_root.volume("vol", NO_OWNER, None).await.unwrap();
        let installed_root =
            Directory::open(&installed_volume, installed_volume.root_directory_object_id())
                .await
                .unwrap();

        for (name, expected_contents) in inner_files.iter() {
            let (object_id, _, _) = installed_root
                .lookup(name)
                .await
                .expect("lookup failed")
                .expect("missing inner file");
            let file =
                ObjectStore::open_object(&installed_volume, object_id, Default::default(), None)
                    .await
                    .unwrap();
            let contents = file.contents(usize::MAX).await.unwrap();
            assert_eq!(contents.as_ref(), expected_contents.as_bytes());
        }

        outer_fs.close().await.unwrap();
    }

    #[fuchsia::test]
    async fn test_install_volume_requires_no_additional_extents() {
        let existing_files = [
            ("file_1", "Hello, world!"),
            ("file_2", "Goodbye, stranger!"),
            ("file_3", "Its been nice..."),
        ];
        let outer_fs = FxFilesystem::open(
            create_test_filesystem(1024, 4096, "vol", &existing_files, false).await,
        )
        .await
        .unwrap();
        let outer_root = root_volume(outer_fs.clone()).await.unwrap();
        let outer_volume = outer_root.volume("vol", NO_OWNER, None).await.unwrap();
        let outer_dir =
            Directory::open(&outer_volume, outer_volume.root_directory_object_id()).await.unwrap();

        let inner_fs = create_test_filesystem(512, 4096, INNER_VOLUME_NAME, &[], true).await;

        let image_handle = {
            let keys: crate::object_store::transaction::LockKeys =
                lock_keys![LockKey::object(outer_volume.store_object_id(), outer_dir.object_id())];
            let mut transaction = outer_volume
                .filesystem()
                .clone()
                .new_transaction(keys, Default::default())
                .await
                .unwrap();
            let handle =
                outer_dir.create_child_file(&mut transaction, "volume_to_install").await.unwrap();
            transaction.commit().await.unwrap();
            handle
        };

        write_image_to_handle(&inner_fs, &image_handle).await;
        let err =
            outer_volume.install_inner_volume(image_handle, INNER_VOLUME_NAME).await.unwrap_err();
        assert_eq!(err.downcast::<FxfsError>().unwrap(), FxfsError::Internal);

        outer_fs.close().await.unwrap();
    }
}
