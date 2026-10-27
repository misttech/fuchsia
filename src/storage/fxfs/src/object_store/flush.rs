// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This module is responsible for flushing (a.k.a. compacting) the object store trees.

use crate::errors::FxfsError;
use crate::filesystem::{FlushReason, ForceMajor};
use crate::log::*;
use crate::lsm_tree::types::{ItemRef, LayerIterator, MaybeContainsKey};
use crate::lsm_tree::{LSMTree, LockedLayer, layer_from_handle};
use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, ReadObjectHandle};
use crate::object_store::extent_record::ExtentValue;
use crate::object_store::object_manager::{ObjectManager, ReservationUpdate};
use crate::object_store::object_record::{ObjectKey, ObjectValue};
use crate::object_store::transaction::{
    AssociatedObject, LockKey, Mutation, ReservationOptions, lock_keys,
};
use crate::object_store::{
    AssocObj, DirectWriter, EncryptedMutations, HandleOptions, LastObjectId, LastObjectIdInfo,
    LockState, MAX_ENCRYPTED_MUTATIONS_SIZE, ObjectStore, Options, ReservedId, StoreInfo,
    layer_size_from_encrypted_mutations_size, tree,
};
use crate::serialized_types::{LATEST_VERSION, Version, VersionedLatest};
use anyhow::{Context, Error, anyhow, bail};
use fxfs_crypto::UnwrappedKey;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

#[fxfs_trace::trace]
impl ObjectStore {
    /// Takes a flush lock on self and performs the flush with a default
    /// FlushReason::Journal(ForceMajor::False).
    pub async fn flush(&self) -> Result<Version, Error> {
        self.flush_with_reason(FlushReason::Journal(ForceMajor::False)).await
    }

    /// Takes a flush lock on self and performs the flush.
    pub async fn flush_with_reason(&self, reason: FlushReason) -> Result<Version, Error> {
        let filesystem = self.filesystem();

        let keys = lock_keys![LockKey::flush(self.store_object_id())];
        let _guard = filesystem.lock_manager().write_lock(keys).await;

        self.flush_guarded_with_reason(reason).await
    }

    /// Performs a flush while already holding a flush guard on self.
    #[trace("store_object_id" => self.store_object_id)]
    pub async fn flush_guarded_with_reason(&self, reason: FlushReason) -> Result<Version, Error> {
        if self.parent_store.is_none() {
            // Early exit, but still return the earliest version used by a struct in the tree
            return Ok(self.tree.get_earliest_version());
        }

        // After taking the lock, check to see if the store has been deleted. Stores that are
        // unlocked read-only (e.g. during volume fsck) behave like locked stores when flushed:
        // mutations are written to the encrypted mutations file rather than layer files.
        let flush_as_locked = match *self.lock_state.lock() {
            LockState::Deleted => {
                // When we compact, it's possible that the store has been deleted since we gathered
                // the list of stores that need compacting.  This is benign.
                return Ok(LATEST_VERSION);
            }
            LockState::Invalid => {
                // An invalid store can't be flushed (e.g. its store info might have been discarded
                // if locking it failed). Fail before committing anything, rather than panicking
                // later.
                bail!(
                    anyhow!(FxfsError::Internal)
                        .context(format!("Cannot flush invalid store {}", self.store_object_id))
                );
            }
            LockState::Locked | LockState::UnlockedReadOnly(..) => true,
            _ => false,
        };

        let filesystem = self.filesystem();
        let object_manager = filesystem.ensure_local().object_manager();
        let earliest_version = self.tree.get_earliest_version();
        let needs_flush = object_manager.needs_flush(self.store_object_id);
        // If we don't need to do anything for the stated flush purpose, do nothing.
        if (reason == FlushReason::Journal(ForceMajor::False) && !needs_flush)
            || (reason == FlushReason::Journal(ForceMajor::True)
                && !needs_flush
                && (flush_as_locked || self.tree.immutable_layer_count() <= 1))
            || (reason == FlushReason::UpgradeVersion && earliest_version == LATEST_VERSION)
            || (reason == FlushReason::EncryptedMutations
                && self.store_info().unwrap().encrypted_mutations_object_id == INVALID_OBJECT_ID)
        {
            // Early exit, but still return the earliest version used by a struct in the
            // tree.
            return Ok(earliest_version);
        }

        let trace = self.trace.load(Ordering::Relaxed);
        if trace {
            info!(store_id = self.store_object_id(); "OS: begin flush");
        }

        if flush_as_locked {
            self.flush_locked().await.with_context(|| {
                format!("Failed to flush object store {}", self.store_object_id)
            })?;
        } else {
            self.flush_unlocked(reason).await.with_context(|| {
                format!("Failed to flush object store {}", self.store_object_id)
            })?;
        }

        if trace {
            info!(store_id = self.store_object_id(); "OS: end flush");
        }
        if let Some(callback) = &*self.flush_callback.lock() {
            callback(self);
        }

        // NOTE: `num_flushes` must only be incremented while the flush lock is held, as
        // `ObjectStore::unlock` relies on this to detect concurrent flushes.
        let mut counters = self.counters.lock();
        counters.num_flushes += 1;
        counters.last_flush_time = Some(std::time::SystemTime::now());
        // Return the earliest version used by a struct in the tree
        Ok(self.tree.get_earliest_version())
    }

    // Flushes an unlocked store. Returns the layer file sizes.
    async fn flush_unlocked(&self, reason: FlushReason) -> Result<Vec<u64>, Error> {
        struct StoreInfoSnapshot<'a> {
            store: &'a ObjectStore,
            store_info: OnceLock<StoreInfo>,
        }
        impl AssociatedObject for StoreInfoSnapshot<'_> {
            fn will_apply_mutation(
                &self,
                _mutation: &Mutation,
                _object_id: u64,
                _manager: &ObjectManager,
            ) {
                let mut store_info = self.store.store_info().unwrap();

                let lock_state = self.store.lock_state.lock();
                if let LockState::Unlocked { mutations_cipher, .. } = &*lock_state {
                    store_info.mutations_cipher_offset = mutations_cipher.sequence_number();
                }

                self.store_info.set(store_info).unwrap();
            }
        }

        let store_info_snapshot = StoreInfoSnapshot { store: self, store_info: OnceLock::new() };

        let filesystem = self.filesystem();
        let txn_options = Options {
            skip_journal_checks: true,
            reservation: ReservationOptions::Borrowed,
            ..Default::default()
        };

        // The BeginFlush mutation must be within a transaction that has no impact on StoreInfo
        // since we want to get an accurate snapshot of StoreInfo.
        let mut transaction = self.new_transaction(lock_keys![], txn_options).await?;
        transaction.add_with_object(
            self.store_object_id(),
            Mutation::BeginFlush,
            AssocObj::Borrowed(&store_info_snapshot),
        );
        transaction.commit().await.context("Failed to commit BeginFlush transaction")?;

        let mut new_store_info = store_info_snapshot.store_info.into_inner().unwrap();

        // There is a transaction to create objects at the start and then another transaction at the
        // end. Between those two transactions, there are transactions that write to the files.  In
        // the first transaction, objects are created in the graveyard. Upon success, the objects
        // are removed from the graveyard.
        let mut transaction = self.new_transaction(lock_keys![], txn_options).await?;

        // Create and write a new layer, compacting existing layers.
        let parent_store = self.parent_store.as_ref().unwrap();
        let handle_options = HandleOptions { skip_journal_checks: true, ..Default::default() };
        let id_and_key = {
            let mut lock_state = self.lock_state.lock();
            match &mut *lock_state {
                LockState::Unlocked { cached_keys, .. } => {
                    if let Some(item) = cached_keys.pop() {
                        Ok(Some(item))
                    } else {
                        log::warn!("No cached keys for flush for store {}", self.store_object_id());
                        Err(anyhow!(FxfsError::Internal).context("No cached keys for flush"))
                    }
                }
                LockState::UnlockedReadOnly(..) => {
                    Err(anyhow!(FxfsError::Internal).context("Flush on read-only store"))
                }
                LockState::Unencrypted => Ok(None),
                _ => Err(anyhow!(FxfsError::Internal))
                    .with_context(|| format!("Invalid lock state ({:?}) for flush", *lock_state)),
            }
        }?;

        let (new_object_tree_layer, unwrapped_key) = if let Some((raw_id, key, unwrapped_key)) =
            id_and_key
        {
            let object_id = ReservedId::new(parent_store, raw_id);
            let handle = ObjectStore::create_object_with_key(
                parent_store,
                &mut transaction,
                object_id,
                handle_options,
                key,
                UnwrappedKey::new(unwrapped_key.clone()),
            )
            .await?;
            (handle, Some(unwrapped_key))
        } else {
            let handle =
                ObjectStore::create_object(parent_store, &mut transaction, handle_options, None)
                    .await?;
            (handle, None)
        };
        let writer = DirectWriter::new(&new_object_tree_layer, txn_options).await;
        let new_object_tree_layer_object_id = new_object_tree_layer.object_id();
        parent_store.add_to_graveyard(&mut transaction, new_object_tree_layer_object_id);

        transaction.commit().await.context("Failed to commit create layer transaction")?;

        // *Do* the actual compaction.
        let full_compaction = match reason {
            FlushReason::UpgradeVersion | FlushReason::Journal(ForceMajor::True) => true,
            _ => false,
        };
        let (layers_to_keep, old_layers) = tree::flush(
            &self.tree,
            writer,
            matches!(reason, FlushReason::Journal(_))
                .then(|| filesystem.ensure_local().journal().get_compaction_yielder()),
            full_compaction,
        )
        .await
        .context("Failed to flush tree")?;

        // Finalise the compaction.
        let mut new_layers = vec![layer_from_handle(new_object_tree_layer, unwrapped_key).await?];
        new_layers.extend(layers_to_keep.iter().map(|l| (*l).clone()));

        new_store_info.layers = Vec::new();
        for layer in &new_layers {
            if let Some(handle) = layer.handle() {
                new_store_info.layers.push(handle.object_id());
            }
        }

        let reservation_update: ReservationUpdate; // Must live longer than end_transaction.
        let mut end_transaction = parent_store
            .new_transaction(
                lock_keys![LockKey::object(
                    self.parent_store.as_ref().unwrap().store_object_id(),
                    self.store_info_handle_object_id().unwrap(),
                )],
                txn_options,
            )
            .await?;

        parent_store.remove_from_graveyard(&mut end_transaction, new_object_tree_layer_object_id);

        // Move the existing layers we're compacting to the graveyard at the end.
        for layer in &old_layers {
            if let Some(handle) = layer.handle() {
                parent_store.add_to_graveyard(&mut end_transaction, handle.object_id());
            }
        }

        let old_encrypted_mutations_object_id =
            std::mem::replace(&mut new_store_info.encrypted_mutations_object_id, INVALID_OBJECT_ID);
        if old_encrypted_mutations_object_id != INVALID_OBJECT_ID {
            parent_store.add_to_graveyard(&mut end_transaction, old_encrypted_mutations_object_id);
        }

        // `last_object_id` is updated differently to other members of `StoreInfo`.  We must ensure
        // that those fields match the current in-memory values.  See the lengthy comment in
        // `get_next_object_id` for more information.  `end_transaction` has a lock on the same lock
        // that `get_next_object_id` uses, so there's no danger of the key changing now.
        //
        // This might capture object IDs that might be in transactions not yet committed.  In
        // theory, we could do better than this but it's not worth the effort.
        match &mut new_store_info.last_object_id {
            LastObjectIdInfo::Unencrypted { id } => {
                let LastObjectId::Unencrypted { id: in_memory_value } =
                    &*self.last_object_id.lock()
                else {
                    unreachable!()
                };
                *id = *in_memory_value;
            }
            LastObjectIdInfo::Encrypted { id, key } => {
                let LastObjectId::Encrypted { id: in_memory_value, .. } =
                    &*self.last_object_id.lock()
                else {
                    unreachable!()
                };
                *id = *in_memory_value;
                let guard = self.store_info.lock();
                let current_store_info = guard.as_ref().unwrap();
                let LastObjectIdInfo::Encrypted { key: in_memory_value, .. } =
                    &current_store_info.last_object_id
                else {
                    unreachable!()
                };
                *key = in_memory_value.clone();
            }
            LastObjectIdInfo::Low32Bit => {}
        }

        self.write_store_info(&mut end_transaction, &new_store_info).await?;

        let layer_file_sizes = new_layers
            .iter()
            .map(|l| l.handle().map(ReadObjectHandle::get_size).unwrap_or(0))
            .collect::<Vec<u64>>();

        let total_layer_size = layer_file_sizes.iter().sum();

        // Even when initiated as a minor compaction, `tree::flush` merges all layers into a single
        // base layer (`layers_to_keep.is_empty()`) if the existing layer is smaller than
        // `DEFAULT_RECLAIM_SIZE`, which purges all tombstones and resets `store_borrowed` to 0.
        reservation_update = ReservationUpdate::new(total_layer_size);
        let new_borrowed = if layers_to_keep.is_empty() {
            0
        } else {
            let old_merged_size = old_layers
                .iter()
                .map(|l| l.handle().map(ReadObjectHandle::get_size).unwrap_or(0))
                .sum();
            filesystem.ensure_local().object_manager().store_borrowed_for_flush(
                self.store_object_id(),
                old_merged_size,
                layer_file_sizes[0],
                total_layer_size,
            )
        };

        end_transaction.add_with_object(
            self.store_object_id(),
            Mutation::EndFlush(new_borrowed),
            AssocObj::Borrowed(&reservation_update),
        );

        if self.trace.load(Ordering::Relaxed) {
            info!(
                store_id = self.store_object_id(),
                old_layer_count = old_layers.len(),
                new_layer_count = new_layers.len(),
                total_layer_size,
                new_store_info:?;
                "OS: compacting"
            );
        }

        end_transaction
            .commit_with_callback(|_| {
                let mut store_info = self.store_info.lock();
                let info = store_info.as_mut().unwrap();
                info.layers = new_store_info.layers;
                info.encrypted_mutations_object_id = new_store_info.encrypted_mutations_object_id;
                info.mutations_cipher_offset = new_store_info.mutations_cipher_offset;
                self.tree.set_layers(new_layers);
            })
            .await
            .context("Failed to commit EndFlush transaction")?;

        // Now close the layers and purge them.
        for layer in old_layers {
            let object_id = layer.handle().map(|h| h.object_id());
            layer.close_layer().await;
            if let Some(object_id) = object_id {
                parent_store
                    .tombstone_object(object_id, txn_options, None)
                    .await
                    .context("Failed to tombstone old layer")?;
            }
        }

        if old_encrypted_mutations_object_id != INVALID_OBJECT_ID {
            parent_store
                .tombstone_object(old_encrypted_mutations_object_id, txn_options, None)
                .await
                .context("Failed to tombstone old encrypted mutations")?;
        }

        Ok(layer_file_sizes)
    }

    // Flushes a locked store.
    async fn flush_locked(&self) -> Result<(), Error> {
        let filesystem = self.filesystem();
        let txn_options = Options {
            skip_journal_checks: true,
            reservation: ReservationOptions::Borrowed,
            ..Default::default()
        };

        let mut transaction = self.new_transaction(lock_keys![], txn_options).await?;
        transaction.add(self.store_object_id(), Mutation::BeginFlush);
        transaction.commit().await.context("Failed to commit BeginFlush transaction")?;

        let mut new_store_info = self.load_store_info().await?;

        // There is a transaction to create objects at the start and then another transaction at the
        // end. Between those two transactions, there are transactions that write to the files.  In
        // the first transaction, objects are created in the graveyard. Upon success, the objects
        // are removed from the graveyard.
        let mut transaction = self.new_transaction(lock_keys![], txn_options).await?;

        let reservation_update: ReservationUpdate; // Must live longer than end_transaction.
        let handle; // Must live longer than end_transaction.
        let mut end_transaction;

        // We need to either write our encrypted mutations to a new file, or append them to an
        // existing one.
        let parent_store = self.parent_store.as_ref().unwrap();
        handle = if new_store_info.encrypted_mutations_object_id == INVALID_OBJECT_ID {
            let handle = ObjectStore::create_object(
                parent_store,
                &mut transaction,
                HandleOptions { skip_journal_checks: true, ..Default::default() },
                None,
            )
            .await?;
            let oid = handle.object_id();
            end_transaction = parent_store
                .new_transaction(
                    lock_keys![
                        LockKey::object(parent_store.store_object_id(), oid),
                        LockKey::object(
                            parent_store.store_object_id(),
                            self.store_info_handle_object_id().unwrap(),
                        ),
                    ],
                    txn_options,
                )
                .await?;
            new_store_info.encrypted_mutations_object_id = oid;
            parent_store.add_to_graveyard(&mut transaction, oid);
            parent_store.remove_from_graveyard(&mut end_transaction, oid);
            handle
        } else {
            end_transaction = parent_store
                .new_transaction(
                    lock_keys![
                        LockKey::object(
                            parent_store.store_object_id(),
                            new_store_info.encrypted_mutations_object_id,
                        ),
                        LockKey::object(
                            parent_store.store_object_id(),
                            self.store_info_handle_object_id().unwrap(),
                        ),
                    ],
                    txn_options,
                )
                .await?;
            ObjectStore::open_object(
                parent_store,
                new_store_info.encrypted_mutations_object_id,
                HandleOptions { skip_journal_checks: true, ..Default::default() },
                None,
            )
            .await?
        };
        transaction
            .commit()
            .await
            .context("Failed to commit create encrypted mutations transaction")?;

        // Append the encrypted mutations, which need to be read from the journal.
        // This assumes that the journal has no buffered mutations for this store (see Self::lock).
        let journaled = filesystem
            .ensure_local()
            .journal()
            .read_transactions_for_object(self.store_object_id)
            .await
            .context("Failed to read encrypted mutations from journal")?;
        let mut buffer = handle.allocate_buffer(MAX_ENCRYPTED_MUTATIONS_SIZE).await;
        let mut writer = buffer.writer();
        EncryptedMutations::from_replayed_mutations(self.store_object_id, journaled)
            .serialize_with_version(&mut writer)?;
        let len = writer.position();
        handle
            .txn_write(&mut end_transaction, handle.get_size(), buffer.subslice(..len))
            .await
            .context("Failed to write encrypted mutations")?;

        self.write_store_info(&mut end_transaction, &new_store_info)
            .await
            .context("Failed to write store info")?;

        let mut total_layer_size = 0;
        for &oid in &new_store_info.layers {
            total_layer_size += parent_store.get_file_size(oid).await?;
        }
        let old_encrypted_size = layer_size_from_encrypted_mutations_size(handle.get_size());
        let new_encrypted_size =
            layer_size_from_encrypted_mutations_size(handle.get_size() + len as u64);
        total_layer_size += new_encrypted_size;

        // Flushing a locked store appends to the encrypted mutations file without compacting
        // tree layers, increasing the store's layer size from `old_encrypted_size` to
        // `new_encrypted_size`.
        reservation_update = ReservationUpdate::new(total_layer_size);
        let new_borrowed = filesystem.ensure_local().object_manager().store_borrowed_for_flush(
            self.store_object_id(),
            old_encrypted_size,
            new_encrypted_size,
            total_layer_size,
        );

        end_transaction.add_with_object(
            self.store_object_id(),
            Mutation::EndFlush(new_borrowed),
            AssocObj::Borrowed(&reservation_update),
        );

        end_transaction
            .commit_with_callback(|_| {
                // If the store is `UnlockedReadOnly`, `self.store_info` is populated in memory, so
                // keep its `encrypted_mutations_object_id` in sync with the on-disk `StoreInfo`.
                if let Some(info) = &mut *self.store_info.lock() {
                    info.encrypted_mutations_object_id =
                        new_store_info.encrypted_mutations_object_id;
                }
            })
            .await
            .context("Failed to commit EndFlush transaction")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FlushReason, ForceMajor};
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder};
    use crate::lsm_tree::types::{LayerIterator, MaybeContainsKey};
    use crate::object_handle::{
        INVALID_OBJECT_ID, ObjectHandle, ReadObjectHandle, WriteObjectHandle,
    };
    use crate::object_store::allocator::max_extent_size_for_block_size;
    use crate::object_store::directory::{Directory, replace_child};
    use crate::object_store::extent_record::ExtentValue;
    use crate::object_store::journal::JournalOptions;
    use crate::object_store::transaction::{Options, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{
        DirType, HandleOptions, LockKey, NewChildStoreOptions, ObjectKey, ObjectStore, ObjectValue,
        StoreOptions, layer_size_from_encrypted_mutations_size,
    };
    use fxfs_insecure_crypto::new_insecure_crypt;
    use std::collections::HashSet;
    use std::ops::Bound;
    use std::sync::Arc;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    #[fuchsia::test]
    async fn test_flush_when_locked() {
        let device = DeviceHolder::new(FakeDevice::new(8192, 1024));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let crypt = Arc::new(new_insecure_crypt());
        let store = root_volume
            .new_volume(
                "test",
                NewChildStoreOptions {
                    options: StoreOptions { crypt: Some(crypt.clone()), ..StoreOptions::default() },
                    ..NewChildStoreOptions::default()
                },
            )
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let foo = root_dir
            .create_child_file(&mut transaction, "foo")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        // When the volume is first created it will include a new mutations key but we want to test
        // what happens when the encrypted mutations file doesn't contain a new mutations key, so we
        // flush here.
        store.flush().await.expect("flush failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        let bar = root_dir
            .create_child_file(&mut transaction, "bar")
            .await
            .expect("create_child_file failed");
        transaction.commit().await.expect("commit failed");

        store.lock().await.expect("lock failed");

        // Flushing the store whilst locked should create an encrypted mutations file.
        store.flush().await.expect("flush failed");

        // Check the reservation.
        let info = store.load_store_info().await.unwrap();
        let parent_store = store.parent_store().unwrap();
        let mut total_layer_size = 0;
        for &oid in &info.layers {
            total_layer_size +=
                parent_store.get_file_size(oid).await.expect("get_file_size failed");
        }
        assert_ne!(info.encrypted_mutations_object_id, INVALID_OBJECT_ID);
        total_layer_size += layer_size_from_encrypted_mutations_size(
            parent_store
                .get_file_size(info.encrypted_mutations_object_id)
                .await
                .expect("get_file_size failed"),
        );
        assert_eq!(
            fs.object_manager().reservation(store.store_object_id()),
            Some(fs.object_manager().tree_reservation(store.store_object_id(), total_layer_size))
        );

        // Unlocking the store should replay that encrypted mutations file.
        store.unlock(crypt).await.expect("unlock failed");

        ObjectStore::open_object(&store, foo.object_id(), HandleOptions::default(), None)
            .await
            .expect("open_object failed");

        ObjectStore::open_object(&store, bar.object_id(), HandleOptions::default(), None)
            .await
            .expect("open_object failed");

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_major_compaction_frees_reservation_and_merges_layers() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with files to create an initial layer > 512 KiB
        // (DEFAULT_RECLAIM_SIZE).
        let num_files = 2000;
        let mut file_ids = Vec::with_capacity(num_files);
        for i in 0..num_files {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            file_ids.push((file.object_id(), filename));
        }

        // Flush (minor) to create Layer 0.
        store.flush().await.expect("flush failed");

        let info_initial = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info_initial.layers.len(), 1);
        let initial_reservation = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");

        // 2. Delete half the files (1000 files).
        for (oid, filename) in &file_ids[0..1000] {
            let mut transaction = store
                .new_transaction(
                    lock_keys![
                        LockKey::object(store.store_object_id(), root_dir.object_id()),
                        LockKey::object(store.store_object_id(), *oid),
                    ],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let replaced = crate::object_store::directory::replace_child(
                &mut transaction,
                None,
                (&root_dir, filename.as_str()),
            )
            .await
            .expect("replace_child failed");
            assert_matches::assert_matches!(
                replaced,
                crate::object_store::directory::ReplacedChild::Object(id) if id == *oid
            );
            transaction.commit().await.expect("commit failed");
        }

        // 3. Minor flush after deleting 1000 files (FlushReason::Journal(ForceMajor::False)).
        // Minor compaction will NOT merge with the base layer because base layer > 512 KiB.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("minor flush failed");

        let info_after_minor = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_minor.layers.len(),
            2,
            "Minor compaction should keep the base layer, creating 2 layers"
        );
        let res_after_minor = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");
        assert!(
            res_after_minor >= initial_reservation,
            "Minor compaction should not decrease reservation because base layer is kept"
        );

        // 4. Major flush (FlushReason::Journal(ForceMajor::True)).
        // Major compaction must merge ALL layers, purge tombstones, and reduce reservation.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");

        let info_after_major = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_major.layers.len(),
            1,
            "Major compaction should merge all layers into 1"
        );
        let res_after_major = fs
            .object_manager()
            .reservation(store.store_object_id())
            .expect("reservation not found");
        assert!(
            res_after_major < initial_reservation,
            "Major compaction should decrease reservation (was {}, initial was {})",
            res_after_major,
            initial_reservation
        );

        // 5. Verify data integrity.
        // Deleted files must be gone.
        for (oid, filename) in &file_ids[0..1000] {
            assert!(
                root_dir.lookup(filename).await.expect("lookup failed").is_none(),
                "Deleted file {} should not exist",
                oid
            );
        }
        for (oid, filename) in &file_ids[1000..num_files] {
            let res = root_dir.lookup(filename).await.expect("lookup failed");
            assert!(res.is_some(), "Surviving file {} should exist", oid);
            assert_eq!(res.unwrap().0, *oid);
        }

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_major_compaction_without_journal_mutations() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let crypt = Arc::new(new_insecure_crypt());
        let store = root_volume
            .new_volume(
                "test",
                NewChildStoreOptions {
                    options: StoreOptions { crypt: Some(crypt.clone()), ..StoreOptions::default() },
                    ..Default::default()
                },
            )
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // Create initial base layer > 512 KiB.
        for i in 0..2000 {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir.create_child_file(&mut transaction, &filename).await.expect("create failed");
            transaction.commit().await.expect("commit failed");
        }
        store.flush().await.expect("flush failed");

        // Write a small file and do a minor flush to create a second layer.
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        root_dir.create_child_file(&mut transaction, "extra_file").await.expect("create failed");
        transaction.commit().await.expect("commit failed");
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("flush failed");

        let info = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info.layers.len(), 2, "Should have 2 layers before major flush");

        // While unlocked read-only with 2 layers and no journal mutations, a major flush must
        // early-exit rather than calling `flush_locked` and creating an empty encrypted_mutations
        // file.
        store.lock().await.expect("lock failed");
        store.unlock_read_only(crypt.clone()).await.expect("unlock_read_only failed");
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        fs.object_manager()
            .flush(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("read-only major flush failed");
        assert_eq!(
            store
                .load_store_info()
                .await
                .expect("load_store_info failed")
                .encrypted_mutations_object_id,
            INVALID_OBJECT_ID
        );
        store.lock_read_only();
        store.unlock(crypt).await.expect("unlock failed");

        // Now, do a major compaction when there are NO journal mutations in ObjectManager.
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");

        let info_after_major = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_major.layers.len(),
            1,
            "Major flush should merge 2 layers into 1 even without journal mutations"
        );

        // Doing another major flush when already at 1 layer and no mutations should early-exit
        // without rewriting the layer or exhausting cached keys.
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("second major flush failed");
        let info_after_second = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(
            info_after_second.layers, info_after_major.layers,
            "Major flush when at 1 layer with no mutations should be a no-op"
        );

        // Locking the store and doing a major flush with no mutations should also early-exit
        // without creating an empty encrypted_mutations file.
        store.lock().await.expect("lock failed");
        assert!(!fs.object_manager().needs_flush(store.store_object_id()));
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("locked major flush failed");
        let info_after_locked = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info_after_locked.encrypted_mutations_object_id, INVALID_OBJECT_ID);

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_major_compaction_purges_deleted_extents() {
        let device = DeviceHolder::new(FakeDevice::new(32768, 512));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Create a base layer > 512 KiB so minor compaction won't merge with it.
        for i in 0..2000 {
            let filename = format!("file_{:04}_{:<200}", i, i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir.create_child_file(&mut transaction, &filename).await.expect("create failed");
            transaction.commit().await.expect("commit failed");
        }

        // Create a data file with extents.
        let data_oid = {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, "data_file")
                .await
                .expect("create_child_file failed");
            let oid = file.object_id();
            transaction.commit().await.expect("commit failed");
            oid
        };

        let data_file = ObjectStore::open_object(&store, data_oid, HandleOptions::default(), None)
            .await
            .expect("open_object failed");
        {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store.store_object_id(), data_oid)],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let mut buffer = data_file.allocate_buffer(131072).await;
            buffer.fill(0xAB);
            data_file
                .txn_write(&mut transaction, 0, buffer.as_ref())
                .await
                .expect("txn_write failed");
            transaction.commit().await.expect("commit failed");
        }

        // Flush (minor) to commit extents to Layer 0.
        store.flush().await.expect("flush failed");

        let info0 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info0.layers.len(), 1);

        // Truncate data_file to 0 to write extent tombstones (ExtentValue::None).
        data_file.truncate(0).await.expect("truncate failed");

        // Minor flush: writes Layer 1 with extent tombstones on top of Layer 0.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("minor flush failed");
        let info1 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info1.layers.len(), 2, "Minor flush should keep Layer 0 and create Layer 1");

        // Major flush: purges ExtentValue::None tombstones via major_iter and merges into 1 layer.
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");
        let info2 = store.load_store_info().await.expect("load_store_info failed");
        assert_eq!(info2.layers.len(), 1, "Major flush should merge into 1 layer");

        // Verify data_file size is 0.
        assert_eq!(data_file.get_size(), 0);

        fs.close().await.expect("close failed");
    }

    // This test case is a regression test for b/548631578.  It verifies that we will force major
    // compactions when borrowed_metadata_space gets too large relative to metadata_reservation.
    // Minor compactions are not sufficient in all cases to return borrowed metadata space, which
    // can eventually exhuast the metadata reservation and prevent all operations (including
    // compaction itself).
    #[fuchsia::test]
    async fn test_borrow_metadata_space_fails_without_major_compaction() {
        let reclaim_size = 65536;
        // 102,400 blocks of 512 bytes = 50 MiB device.
        let device = DeviceHolder::new(FakeDevice::new(102400, 512));
        let fs = FxFilesystemBuilder::new()
            .journal_options(JournalOptions { reclaim_size, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");

        // The test creates many files in several stores, and then deletes them.  The deletions
        // create additional layer files, which should be collapsed into the base layers (and
        // cancel out the file creations) following a major compaction, which should happen
        // automatically due to the large amount of borrowed space.
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let num_stores = 3;
        let files_per_store = 3500;
        let mut stores_and_files = Vec::new();

        for s in 0..num_stores {
            let store = root_volume
                .new_volume(&format!("test_{s}"), NewChildStoreOptions::default())
                .await
                .expect("new_volume failed");
            let root_dir = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let mut file_ids = Vec::with_capacity(files_per_store);
            for i in 0..files_per_store {
                let filename = format!("file_{:04}_{:<200}", i, i);
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store.store_object_id(), root_dir.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = root_dir
                    .create_child_file(&mut transaction, &filename)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                file_ids.push((file.object_id(), filename));
            }
            store
                .flush_with_reason(FlushReason::Journal(ForceMajor::True))
                .await
                .expect("flush failed");
            stores_and_files.push((store, root_dir, file_ids));
        }

        for (_, root_dir, file_ids) in stores_and_files {
            for (oid, filename) in &file_ids[0..2500] {
                let mut context = root_dir
                    .acquire_context_for_replace(None, filename.as_str(), true)
                    .await
                    .expect("acquire_context failed");
                let replaced = crate::object_store::directory::replace_child(
                    &mut context.transaction,
                    None,
                    (&root_dir, filename.as_str()),
                )
                .await
                .expect("replace_child failed");
                assert_matches::assert_matches!(
                    replaced,
                    crate::object_store::directory::ReplacedChild::Object(id) if id == *oid
                );
                context.transaction.commit().await.expect("commit failed");
            }
        }

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_file_truncate_and_extend_does_not_force_major_compaction() {
        let device = DeviceHolder::new(FakeDevice::new(65536, 512));
        let fs = FxFilesystemBuilder::new()
            .journal_options(JournalOptions { reclaim_size: 32768, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_id = store.store_object_id();
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with old files so that its base layer exceeds 512 KiB
        // (`DEFAULT_RECLAIM_SIZE`).
        for i in 0..2000 {
            let filename = format!("base_{i:04}_{:<200}", i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }

        // Create a working file and write 4 KiB to it.
        let file = {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, "work_file")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            file
        };
        let mut buf = file.allocate_buffer(4096).await;
        buf.fill(0xaa);
        file.write_or_append(Some(0), buf.as_ref()).await.expect("write_or_append failed");
        fs.journal().force_compact().await.expect("force_compact failed");

        let bytes_before_loop = fs.object_manager().compaction_bytes_written();

        // 2. Repeatedly truncate `work_file` to 0 (borrowing metadata space) and write 4 KiB back
        // to `work_file` (funded metadata space), letting the journal compact naturally. Writing
        // 0..4096 again replaces the `ExtentValue::None` record from `truncate`, so after minor
        // compaction the minor layer returns to its original 4 KiB size with all history gone and
        // the > 512 KiB base layer must never be rewritten by a major compaction.
        for _ in 0..450 {
            file.truncate_with_options(
                Options {
                    reservation: crate::object_store::transaction::ReservationOptions::Borrowed,
                    ..Default::default()
                },
                0,
            )
            .await
            .expect("truncate failed");

            file.write_or_append(Some(0), buf.as_ref()).await.expect("write_or_append failed");
        }
        fs.journal().force_compact().await.expect("force_compact failed");

        // Across 450 iterations (~12 journal compactions), minor compactions only write the 4 KiB
        // minor layer (plus small root_store / allocator layers, < 600 KiB total), whereas even a
        // single major compaction of the > 512 KiB base layer adds > 530 KiB.
        let compaction_bytes = fs.object_manager().compaction_bytes_written() - bytes_before_loop;
        assert!(
            compaction_bytes < 600 * 1024,
            "Truncate and extend churn caused unnecessary major compaction: \
             {compaction_bytes} bytes written"
        );

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_temporary_file_create_and_delete_does_not_force_major_compaction() {
        let device = DeviceHolder::new(FakeDevice::new(65536, 512));
        let fs = FxFilesystemBuilder::new()
            .journal_options(JournalOptions { reclaim_size: 32768, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_id = store.store_object_id();
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with old files so that its base layer exceeds 512 KiB
        // (`DEFAULT_RECLAIM_SIZE`).
        for i in 0..2000 {
            let filename = format!("base_{i:04}_{:<200}", i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }
        fs.journal().force_compact().await.expect("force_compact failed");
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");
        fs.journal().force_compact().await.expect("force_compact failed");

        let (base_layer_object_id, base_layer_len) = {
            let layers = store.tree().immutable_layer_set().layers;
            assert_eq!(layers.len(), 1);
            assert!(layers[0].handle().unwrap().get_size() > 512 * 1024);
            (layers[0].handle().unwrap().object_id(), layers[0].len())
        };

        // 2. Repeatedly create temporary files and delete/tombstone them across `NUM_TEMP_FILES`
        // iterations, forcing minor compactions between creation and deletion periodically so that
        // the creation records and tombstones span separate minor layers before merging.
        const NUM_TEMP_FILES: usize = 200;
        const MINOR_FLUSH_INTERVAL: usize = 25;
        let graveyard_dir_id = store.graveyard_directory_object_id();
        let mut tombstone_keys = Vec::with_capacity(NUM_TEMP_FILES * 3);
        for i in 0..NUM_TEMP_FILES {
            let filename = format!("tmp_{i:04}");
            let oid = {
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store_id, root_dir.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = root_dir
                    .create_child_file(&mut transaction, &filename)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                file.object_id()
            };

            // Periodically flush right after creating the file so its live records are written to
            // a persistent minor layer before it is deleted. That forces subsequent minor
            // compactions to merge creation and tombstone records across separate persistent minor
            // layers, while the remaining iterations exercise creation and deletion within the same
            // mutable layer between journal-triggered compactions.
            if i % MINOR_FLUSH_INTERVAL == 0 {
                store
                    .flush_with_reason(FlushReason::Journal(ForceMajor::False))
                    .await
                    .expect("minor flush failed");
            }

            {
                let mut context = root_dir
                    .acquire_context_for_replace(None, filename.as_str(), true)
                    .await
                    .expect("acquire_context failed");
                replace_child(&mut context.transaction, None, (&root_dir, filename.as_str()))
                    .await
                    .expect("replace_child failed");
                context.transaction.commit().await.expect("commit failed");
            }
            fs.graveyard().queue_tombstone_object(store_id, oid);
            fs.graveyard().flush().await;

            tombstone_keys.push(ObjectKey::object(oid));
            tombstone_keys.push(ObjectKey::child(root_dir.object_id(), &filename, DirType::Normal));
            tombstone_keys.push(ObjectKey::graveyard_entry(graveyard_dir_id, oid));
        }
        fs.journal().force_compact().await.expect("force_compact failed");

        // Almost all temporary file records and tombstones should have been eliminated during minor
        // compactions via the base layer's bloom filter without ever rewriting the > 512 KiB base
        // layer.
        {
            let layers = store.tree().immutable_layer_set().layers;
            assert_eq!(layers.len(), 2);

            // Every compaction allocates a fresh layer object ID and moves old layers to the
            // graveyard, so matching `base_layer_object_id` proves no major compaction occurred.
            assert_eq!(layers[1].handle().unwrap().object_id(), base_layer_object_id);
            assert_eq!(layers[1].len(), base_layer_len);

            // Without minor-compaction tombstone elision, `layers[0]` would contain
            // `1 + 3 * NUM_TEMP_FILES` (601) items (`root_dir` plus `Object`, `Child`, and
            // `GraveyardEntry` tombstones for all 200 deleted files). With tombstone elision,
            // `layers[0]` retains only `root_dir` plus whichever tombstones hit a false positive in
            // `layers[1]`'s bloom filter.
            let expected_false_positives = tombstone_keys
                .iter()
                .filter(|key| layers[1].maybe_contains_key(key) != MaybeContainsKey::False)
                .count();
            assert_eq!(layers[0].len(), 1 + expected_false_positives);

            // `layers[1]`'s bloom filter targets a 0.1% false-positive rate (`TARGET_FP_RATE =
            // 0.001`), so across 600 tombstone keys the expected false-positive count is < 0.6
            // (and P(> 15) < 1e-19), compared to 601 items without tombstone elision.
            assert!(
                layers[0].len() <= 16,
                "unexpectedly many items in minor layer: {}",
                layers[0].len()
            );

            let mut iter = layers[0].seek(Bound::Unbounded).await.expect("seek failed");
            let mut saw_root_dir = false;
            let mut count = 0;
            while let Some(item) = iter.get() {
                count += 1;
                if item.key == &ObjectKey::object(root_dir.object_id()) {
                    saw_root_dir = true;
                } else {
                    // Any other surviving item in the minor layer must be a tombstone kept due to a
                    // bloom filter false positive against the base layer.
                    assert!(
                        matches!(
                            item.value,
                            ObjectValue::None | ObjectValue::Extent(ExtentValue::None)
                        ),
                        "unexpected non-tombstone item in minor layer: {item:?}"
                    );
                    assert_ne!(
                        layers[1].maybe_contains_key(item.key),
                        MaybeContainsKey::False,
                        "tombstone survived despite base layer bloom filter returning False: \
                         {item:?}"
                    );
                }
                iter.advance().await.expect("advance failed");
            }
            assert!(saw_root_dir, "expected root_dir item in minor layer");
            assert_eq!(count, layers[0].len());
        }

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_minor_compaction_extent_tombstones() {
        const MIB: u64 = 1024 * 1024;
        let device = DeviceHolder::new(FakeDevice::new(16384, 4096));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_id = store.store_object_id();
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with 2000 files so its base layer exceeds 512 KiB
        // (`DEFAULT_RECLAIM_SIZE`), plus four files exercising multi-MiB extent ranges across the
        // 1 MiB fuzzy-hash buckets (`EXTENT_HASH_BUCKET_SIZE`) and `MAX_HASH_PARTITIONS` (4):
        //   - `large_base_file`: single contiguous maximum-sized (`1.75 MiB` for 4 KiB blocks)
        //     extent at `512 KiB..2.25 MiB`, spanning 3 buckets (`[0..1M)`, `[1..2M)`, `[2..3M)`).
        //   - `tail_4m_file`: 4 KiB extent only in the 4th bucket (`3.5 MiB..3.5 MiB + 4 KiB`).
        //   - `tail_8m_file`: 4 KiB extent only in the 8th bucket (`7.5 MiB..7.5 MiB + 4 KiB`).
        //   - `prefix_base_file`: 64 KiB extent only in the 1st bucket (`0..64 KiB`).
        for i in 0..2000 {
            let filename = format!("base_{i:04}_{:<200}", i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }

        let create_file = |name: &'static str| {
            let store = &store;
            let root_dir = &root_dir;
            async move {
                let mut transaction = store
                    .new_transaction(
                        lock_keys![LockKey::object(store_id, root_dir.object_id())],
                        Options::default(),
                    )
                    .await
                    .expect("new_transaction failed");
                let file = root_dir
                    .create_child_file(&mut transaction, name)
                    .await
                    .expect("create_child_file failed");
                transaction.commit().await.expect("commit failed");
                file
            }
        };

        let large_base_len = max_extent_size_for_block_size(fs.block_size());
        assert_eq!(large_base_len, 1792 * 1024);
        let large_base_start = 512 * 1024;
        let large_base_end = large_base_start + large_base_len;
        let large_base_file = create_file("large_base_file").await;
        {
            let mut buf = large_base_file.allocate_buffer(large_base_len as usize).await;
            buf.fill(0xaa);
            large_base_file
                .write_or_append(Some(large_base_start), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }

        let tail_4m_offset = 3 * MIB + 512 * 1024;
        let tail_4m_file = create_file("tail_4m_file").await;
        {
            let mut buf = tail_4m_file.allocate_buffer(4096).await;
            buf.fill(0xbb);
            tail_4m_file
                .write_or_append(Some(tail_4m_offset), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }

        let tail_8m_offset = 7 * MIB + 512 * 1024;
        let tail_8m_file = create_file("tail_8m_file").await;
        {
            let mut buf = tail_8m_file.allocate_buffer(4096).await;
            buf.fill(0xcc);
            tail_8m_file
                .write_or_append(Some(tail_8m_offset), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }

        let prefix_base_file = create_file("prefix_base_file").await;
        {
            let mut buf = prefix_base_file.allocate_buffer(64 * 1024).await;
            buf.fill(0xdd);
            prefix_base_file
                .write_or_append(Some(0), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }

        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::True))
            .await
            .expect("major flush failed");
        let base_layer_object_id = {
            let layers = store.tree().immutable_layer_set().layers;
            assert_eq!(layers.len(), 1);
            assert!(layers[0].handle().unwrap().get_size() > 512 * 1024);

            // Confirm `large_base_file`'s 1.75 MiB allocation is stored as a single contiguous
            // extent record spanning 3 fuzzy-hash buckets (`[0..1M)`, `[1..2M)`, `[2..3M)`) in the
            // base layer.
            let expected_key = ObjectKey::extent(
                large_base_file.object_id(),
                large_base_file.attribute_id(),
                large_base_start..large_base_end,
            );
            let iter =
                layers[0].seek(Bound::Included(&expected_key)).await.expect("base seek failed");
            assert_eq!(iter.get().map(|item| item.key), Some(&expected_key));

            layers[0].handle().unwrap().object_id()
        };

        // 2. Write new multi-MiB extents in ranges that have no records in the base layer (`1..4
        // MiB` on `prefix_base_file` and `0..4 MiB` on a newly created `temp_extent_file`), and
        // flush them into a persistent minor layer.
        {
            let mut buf = prefix_base_file.allocate_buffer((3 * MIB) as usize).await;
            buf.fill(0xee);
            prefix_base_file
                .write_or_append(Some(MIB), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }
        let temp_extent_file = create_file("temp_extent_file").await;
        {
            let mut buf = temp_extent_file.allocate_buffer((4 * MIB) as usize).await;
            buf.fill(0xff);
            temp_extent_file
                .write_or_append(Some(0), buf.as_ref())
                .await
                .expect("write_or_append failed");
        }
        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("first minor flush failed");

        // 3. Delete/zero extent ranges across all five files and run another minor compaction:
        //   - On `large_base_file` (`512 KiB..2.25 MiB` in base): punch holes at the start in
        //     bucket 0 (`512..516 KiB`), strictly inside interior bucket 1 (`1.5 MiB..1.5 MiB +
        //     4 KiB`), straddling the bucket 1/2 boundary (`2 MiB - 4 KiB..2 MiB + 4 KiB`), and at
        //     the very end in bucket 2 (`2.25 MiB - 4 KiB..2.25 MiB`).
        //   - On `tail_4m_file`: zero `0..4 MiB` (4 buckets == `MAX_HASH_PARTITIONS`, overlapping
        //     the base extent only in the 4th bucket).
        //   - On `tail_8m_file`: zero `0..8 MiB` (8 buckets > `MAX_HASH_PARTITIONS`, returning
        //     `RangeKeyTooLarge` and overlapping the base extent only in the 8th bucket).
        //   - On `prefix_base_file`: zero `1..4 MiB` (buckets 1..4 are absent from the base layer,
        //     even though bucket 0 is present for `prefix_base_file`).
        //   - On `temp_extent_file`: truncate to 0 (emitting `0..4 MiB` `ExtentValue::None` across
        //     4 buckets, all absent from the base layer).
        let hole_start = large_base_start..(large_base_start + 4096);
        let hole_interior = (MIB + 512 * 1024)..(MIB + 512 * 1024 + 4096);
        let hole_boundary = (2 * MIB - 4096)..(2 * MIB + 4096);
        let hole_end = (large_base_end - 4096)..large_base_end;
        for hole in
            [hole_start.clone(), hole_interior.clone(), hole_boundary.clone(), hole_end.clone()]
        {
            let mut transaction =
                large_base_file.new_transaction().await.expect("new_transaction failed");
            large_base_file.zero(&mut transaction, hole).await.expect("zero failed");
            transaction.commit().await.expect("commit failed");
        }
        {
            let mut transaction =
                tail_4m_file.new_transaction().await.expect("new_transaction failed");
            tail_4m_file.zero(&mut transaction, 0..4 * MIB).await.expect("zero failed");
            transaction.commit().await.expect("commit failed");
        }
        {
            let mut transaction =
                tail_8m_file.new_transaction().await.expect("new_transaction failed");
            tail_8m_file.zero(&mut transaction, 0..8 * MIB).await.expect("zero failed");
            transaction.commit().await.expect("commit failed");
        }
        {
            let mut transaction =
                prefix_base_file.new_transaction().await.expect("new_transaction failed");
            prefix_base_file.zero(&mut transaction, MIB..4 * MIB).await.expect("zero failed");
            transaction.commit().await.expect("commit failed");
        }
        temp_extent_file.truncate(0).await.expect("truncate failed");

        store
            .flush_with_reason(FlushReason::Journal(ForceMajor::False))
            .await
            .expect("second minor flush failed");

        // 4. Verify `maybe_contains_key` behavior and surviving `ExtentValue::None` records in the
        // compacted minor layer (`layers[0]`), confirming no major compaction occurred.
        let must_keep_keys = [
            ObjectKey::extent(
                large_base_file.object_id(),
                large_base_file.attribute_id(),
                hole_start.clone(),
            ),
            ObjectKey::extent(
                large_base_file.object_id(),
                large_base_file.attribute_id(),
                hole_interior.clone(),
            ),
            ObjectKey::extent(
                large_base_file.object_id(),
                large_base_file.attribute_id(),
                hole_boundary.clone(),
            ),
            ObjectKey::extent(
                large_base_file.object_id(),
                large_base_file.attribute_id(),
                hole_end.clone(),
            ),
            ObjectKey::extent(tail_4m_file.object_id(), tail_4m_file.attribute_id(), 0..4 * MIB),
            ObjectKey::extent(tail_8m_file.object_id(), tail_8m_file.attribute_id(), 0..8 * MIB),
        ];
        let elidable_keys = [
            ObjectKey::extent(
                prefix_base_file.object_id(),
                prefix_base_file.attribute_id(),
                MIB..4 * MIB,
            ),
            ObjectKey::extent(
                temp_extent_file.object_id(),
                temp_extent_file.attribute_id(),
                0..4 * MIB,
            ),
        ];

        {
            let layers = store.tree().immutable_layer_set().layers;
            assert_eq!(layers.len(), 2);
            assert_eq!(layers[1].handle().unwrap().object_id(), base_layer_object_id);

            // All 5 `<= 4`-bucket overlapping keys must return `Maybe` against the base layer
            // (covering start, interior, boundary, and end buckets of `large_base_file` as well as
            // `0..4 MiB` hitting only in the 4th bucket of `tail_4m_file`).
            for key in &must_keep_keys[..5] {
                assert_eq!(layers[1].maybe_contains_key(key), MaybeContainsKey::Maybe, "{key:?}");
            }

            // The 8-bucket key on `tail_8m_file` exceeds `MAX_HASH_PARTITIONS` (4) and must return
            // `RangeKeyTooLarge`.
            assert_eq!(
                layers[1].maybe_contains_key(&must_keep_keys[5]),
                MaybeContainsKey::RangeKeyTooLarge
            );

            let mut minor_deleted_extents = HashSet::new();
            let mut iter = layers[0].seek(Bound::Unbounded).await.expect("seek failed");
            while let Some(item) = iter.get() {
                if matches!(item.value, ObjectValue::Extent(ExtentValue::None)) {
                    minor_deleted_extents.insert(item.key.clone());
                }
                iter.advance().await.expect("advance failed");
            }

            for key in &must_keep_keys {
                assert!(
                    minor_deleted_extents.contains(key),
                    "expected overlapping/large extent tombstone to be kept: {key:?}"
                );
            }
            for key in &elidable_keys {
                let expected_kept = layers[1].maybe_contains_key(key) != MaybeContainsKey::False;
                assert_eq!(
                    minor_deleted_extents.contains(key),
                    expected_kept,
                    "unexpected retention state for non-overlapping extent tombstone: {key:?}"
                );
            }
        }

        // 5. Read back file contents to ensure none of the zeroed base-layer ranges were
        // resurrected and live base-layer data remains intact.
        let mut buf_4k = large_base_file.allocate_buffer(4096).await;
        let mut buf_8k = large_base_file.allocate_buffer(8192).await;

        assert_eq!(
            large_base_file
                .read_aligned(hole_start.start, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_start.end, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);

        assert_eq!(
            large_base_file
                .read_aligned(hole_interior.start - 4096, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_interior.start, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_interior.end, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);

        assert_eq!(
            large_base_file
                .read_aligned(hole_boundary.start - 4096, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_boundary.start, buf_8k.as_mut())
                .await
                .expect("read_aligned failed"),
            8192
        );
        assert_eq!(buf_8k.as_ptr_slice().to_vec(), vec![0u8; 8192]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_boundary.end, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);

        assert_eq!(
            large_base_file
                .read_aligned(hole_end.start - 4096, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0xaa; 4096]);
        assert_eq!(
            large_base_file
                .read_aligned(hole_end.start, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);

        assert_eq!(
            tail_4m_file
                .read_aligned(tail_4m_offset, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);

        assert_eq!(
            tail_8m_file
                .read_aligned(tail_8m_offset, buf_4k.as_mut())
                .await
                .expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);

        let mut buf_64k = prefix_base_file.allocate_buffer(64 * 1024).await;
        assert_eq!(
            prefix_base_file.read_aligned(0, buf_64k.as_mut()).await.expect("read_aligned failed"),
            64 * 1024
        );
        assert_eq!(buf_64k.as_ptr_slice().to_vec(), vec![0xdd; 64 * 1024]);
        assert_eq!(
            prefix_base_file.read_aligned(MIB, buf_4k.as_mut()).await.expect("read_aligned failed"),
            4096
        );
        assert_eq!(buf_4k.as_ptr_slice().to_vec(), vec![0u8; 4096]);
        assert_eq!(temp_extent_file.get_size(), 0);

        fs.close().await.expect("close failed");
    }

    #[fuchsia::test]
    async fn test_mixed_deletes_and_creates_reclaim_space_via_major_compaction() {
        let device = DeviceHolder::new(FakeDevice::new(16384, 512));
        let fs = FxFilesystemBuilder::new()
            .journal_options(JournalOptions { reclaim_size: 32768, ..Default::default() })
            .format(true)
            .open(device)
            .await
            .expect("open failed");
        let root_volume = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_volume
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("new_volume failed");
        let store_id = store.store_object_id();
        let root_dir =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        // 1. Populate the store with 2000 files so its base layer exceeds 512 KiB.
        let num_files = 2000;
        let mut file_ids = Vec::with_capacity(num_files);
        for i in 0..num_files {
            let filename = format!("file_{i:04}_{:<200}", i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, &filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            file_ids.push((file.object_id(), filename));
        }
        fs.journal().force_compact().await.expect("force_compact failed");
        fs.journal().force_compact().await.expect("force_compact failed");

        // Create a filler file, fill the disk, and then truncate 512 KiB off `filler` so the
        // filesystem has ~512 KiB of free space headroom.
        let filler = {
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let file = root_dir
                .create_child_file(&mut transaction, "filler")
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
            file
        };
        let chunk = filler.allocate_buffer(65536).await;
        let mut offset = 0;
        while filler.write_or_append(Some(offset), chunk.as_ref()).await.is_ok() {
            offset += 65536;
        }
        filler.truncate(offset - 512 * 1024).await.expect("truncate filler failed");
        fs.journal().force_compact().await.expect("force_compact failed");

        // 2. Repeatedly delete an old file and create a new file 1500 times (keeping the live file
        // count constant at 2000). Because the deleted files reside in the base layer, major
        // compaction must run periodically to purge the dead base-layer records and minor-layer
        // tombstones; otherwise the accumulating minor layers and their metadata reservations will
        // exhaust the 512 KiB of free space and fail with `NoSpace`.
        for (i, (oid, filename)) in file_ids[0..1500].iter().enumerate() {
            {
                let mut context = root_dir
                    .acquire_context_for_replace(None, filename.as_str(), true)
                    .await
                    .expect("acquire_context failed");
                replace_child(&mut context.transaction, None, (&root_dir, filename.as_str()))
                    .await
                    .expect("replace_child failed");
                context.transaction.commit().await.expect("commit failed");
            }
            fs.graveyard().queue_tombstone_object(store_id, *oid);
            fs.graveyard().flush().await;

            let new_filename = format!("new_{i:04}_{:<200}", i);
            let mut transaction = store
                .new_transaction(
                    lock_keys![LockKey::object(store_id, root_dir.object_id())],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            root_dir
                .create_child_file(&mut transaction, &new_filename)
                .await
                .expect("create_child_file failed");
            transaction.commit().await.expect("commit failed");
        }

        fs.close().await.expect("close failed");
    }
}

impl tree::MajorCompactable<ObjectKey, ObjectValue> for LSMTree<ObjectKey, ObjectValue> {
    async fn major_iter(
        iter: impl LayerIterator<ObjectKey, ObjectValue>,
    ) -> Result<impl LayerIterator<ObjectKey, ObjectValue>, Error> {
        iter.filter(|item: ItemRef<'_, _, _>| match item {
            // Object Tombstone.
            ItemRef { value: ObjectValue::None, .. } => false,
            // Deleted extent.
            ItemRef { value: ObjectValue::Extent(ExtentValue::None), .. } => false,
            _ => true,
        })
        .await
    }

    async fn minor_iter(
        iter: impl LayerIterator<ObjectKey, ObjectValue>,
        layers_to_keep: impl AsRef<[LockedLayer<ObjectKey, ObjectValue>]> + Send + Sync,
    ) -> Result<impl LayerIterator<ObjectKey, ObjectValue>, Error> {
        iter.filter(move |item: ItemRef<'_, _, _>| match item {
            // Object Tombstone or deleted extent: only keep if a kept layer might contain the key.
            ItemRef { key, value: ObjectValue::None | ObjectValue::Extent(ExtentValue::None) } => {
                layers_to_keep
                    .as_ref()
                    .iter()
                    .any(|layer| layer.maybe_contains_key(key) != MaybeContainsKey::False)
            }
            _ => true,
        })
        .await
    }
}
