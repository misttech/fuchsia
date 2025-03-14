// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module contains types used for sending and receiving messages to and from the storage
//! agent.

use crate::base::{SettingInfo, SettingType};
use fuchsia_trace as ftrace;
use settings_storage::UpdateState;

/// `Payload` wraps the request and response payloads.
#[derive(Clone, PartialEq, Debug)]
pub enum Payload {
    Request(StorageRequest),
    Response(StorageResponse),
}

/// `StorageRequest` contains all of the requests that can be made to the storage agent.
#[derive(Clone, PartialEq, Debug)]
pub enum StorageRequest {
    /// A read requests for the corresponding [`StorageInfo`] of this `StorageType`.
    Read(StorageType, ftrace::Id),
    /// A write requests for this [`StorageInfo`].
    Write(StorageInfo, ftrace::Id),
}

#[derive(Clone, PartialEq, Debug)]
pub enum StorageType {
    SettingType(SettingType),
}

impl From<SettingType> for StorageType {
    fn from(setting_type: SettingType) -> Self {
        StorageType::SettingType(setting_type)
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum StorageInfo {
    SettingInfo(SettingInfo),
}

impl From<SettingInfo> for StorageInfo {
    fn from(setting_info: SettingInfo) -> Self {
        StorageInfo::SettingInfo(setting_info)
    }
}

/// `StorageResponse` contains the corresponding result types to the matching [`StorageRequest`]
/// variants of the same name.
#[derive(Clone, PartialEq, Debug)]
pub enum StorageResponse {
    /// The storage info read from storage in response to a [`StorageRequest::Read`]
    Read(StorageInfo),
    /// The result of a write request with either the [`UpdateState`] after a successful write
    /// or a formatted error describing why the write could not occur.
    Write(Result<UpdateState, Error>),
}

/// `Error` encapsulates a formatted error the occurs due to write failures.
#[derive(Clone, PartialEq, Debug)]
pub struct Error {
    /// The error message.
    pub message: String,
}

#[cfg(test)]
pub(crate) mod testing {
    use anyhow::Error;
    use fidl_fuchsia_stash::{
        StoreAccessorMarker, StoreAccessorProxy, StoreAccessorRequest, Value,
    };
    use fuchsia_async as fasync;
    use fuchsia_inspect::component;
    use futures::lock::Mutex;
    use futures::prelude::*;
    use serde::{Deserialize, Serialize};
    use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
    use settings_storage::stash_logger::StashInspectLogger;
    use settings_storage::storage_factory::{
        DefaultLoader, InitializationState, NoneT, StorageAccess, StorageFactory,
    };
    use std::any::Any;
    use std::collections::HashMap;
    use std::rc::Rc;

    #[derive(PartialEq)]
    pub(crate) enum StashAction {
        Get,
        Flush,
        Set,
    }

    pub(crate) struct StashStats {
        actions: Vec<StashAction>,
    }

    impl StashStats {
        pub(crate) fn new() -> Self {
            StashStats { actions: Vec::new() }
        }

        pub(crate) fn record(&mut self, action: StashAction) {
            self.actions.push(action);
        }
    }

    /// Storage that does not write to disk, for testing.
    pub(crate) struct InMemoryStorageFactory {
        initial_data: HashMap<&'static str, String>,
        device_storage_cache: Mutex<InitializationState<DeviceStorage>>,
        inspect_handle: Rc<Mutex<StashInspectLogger>>,
    }

    impl Default for InMemoryStorageFactory {
        fn default() -> Self {
            Self::new()
        }
    }

    const INITIALIZATION_ERROR: &str =
        "Cannot initialize an already accessed device storage. Make \
        sure you're not retrieving a DeviceStorage before passing InMemoryStorageFactory to an \
        EnvironmentBuilder. That must be done after. If you need initial data, use \
        InMemoryStorageFactory::with_initial_data";

    impl InMemoryStorageFactory {
        /// Constructs a new `InMemoryStorageFactory` with the ability to create a [`DeviceStorage`]
        /// that can only read and write to the storage keys passed in.
        pub fn new() -> Self {
            InMemoryStorageFactory {
                initial_data: HashMap::new(),
                device_storage_cache: Mutex::new(InitializationState::new()),
                inspect_handle: Rc::new(Mutex::new(StashInspectLogger::new(
                    component::inspector().root(),
                ))),
            }
        }

        /// Constructs a new `InMemoryStorageFactory` with the data written to stash. This simulates
        /// the data existing in storage before the RestoreAgent reads it.
        pub fn with_initial_data<T>(data: &T) -> Self
        where
            T: DeviceStorageCompatible,
        {
            let mut map = HashMap::new();
            let _ = map.insert(T::KEY, serde_json::to_string(data).unwrap());
            InMemoryStorageFactory {
                initial_data: map,
                device_storage_cache: Mutex::new(InitializationState::new()),
                inspect_handle: Rc::new(Mutex::new(StashInspectLogger::new(
                    component::inspector().root(),
                ))),
            }
        }

        /// Helper method to simplify setup for `InMemoryStorageFactory` in tests.
        pub(crate) async fn initialize_storage<T>(&self)
        where
            T: DeviceStorageCompatible,
        {
            self.initialize_storage_for_key(T::KEY).await;
        }

        async fn initialize_storage_for_key(&self, key: &'static str) {
            match &mut *self.device_storage_cache.lock().await {
                InitializationState::Initializing(initial_keys, _) => {
                    let _ = initial_keys.insert(key, None);
                }
                InitializationState::Initialized(_) => panic!("{}", INITIALIZATION_ERROR),
                _ => unreachable!(),
            }
        }

        async fn initialize_storage_for_key_with_loader(
            &self,
            key: &'static str,
            loader: Box<dyn Any>,
        ) {
            match &mut *self.device_storage_cache.lock().await {
                InitializationState::Initializing(initial_keys, _) => {
                    let _ = initial_keys.insert(key, Some(loader));
                }
                InitializationState::Initialized(_) => panic!("{}", INITIALIZATION_ERROR),
                _ => unreachable!(),
            }
        }

        /// Retrieve the [`DeviceStorage`] singleton.
        pub(crate) async fn get_device_storage(&self) -> Rc<DeviceStorage> {
            let initialization = &mut *self.device_storage_cache.lock().await;
            match initialization {
                InitializationState::Initializing(initial_keys, _) => {
                    let mut device_storage = DeviceStorage::with_stash_proxy(
                        initial_keys.drain(),
                        || {
                            let (stash_proxy, _) = spawn_stash_proxy();
                            stash_proxy
                        },
                        Rc::clone(&self.inspect_handle),
                    );
                    device_storage.set_caching_enabled(false);
                    device_storage.set_debounce_writes(false);

                    // write initial data to storage
                    for (&key, data) in &self.initial_data {
                        device_storage
                            .write_str(key, data.clone())
                            .await
                            .expect("Failed to write initial data");
                    }

                    let device_storage = Rc::new(device_storage);
                    *initialization = InitializationState::Initialized(Rc::clone(&device_storage));
                    device_storage
                }
                InitializationState::Initialized(device_storage) => Rc::clone(device_storage),
                _ => unreachable!(),
            }
        }
    }

    impl StorageFactory for InMemoryStorageFactory {
        type Storage = DeviceStorage;

        async fn initialize<T>(&self) -> Result<(), Error>
        where
            T: StorageAccess<Storage = DeviceStorage>,
        {
            self.initialize_storage_for_key(T::STORAGE_KEY).await;
            Ok(())
        }

        async fn initialize_with_loader<T, L>(&self, loader: L) -> Result<(), Error>
        where
            T: StorageAccess<Storage = DeviceStorage>,
            L: DefaultLoader<Result = T::Data> + 'static,
        {
            self.initialize_storage_for_key_with_loader(
                T::STORAGE_KEY,
                Box::new(loader) as Box<dyn Any>,
            )
            .await;
            Ok(())
        }

        async fn get_store(&self) -> Rc<DeviceStorage> {
            self.get_device_storage().await
        }
    }

    fn spawn_stash_proxy() -> (StoreAccessorProxy, Rc<Mutex<StashStats>>) {
        let (stash_proxy, mut stash_stream) =
            fidl::endpoints::create_proxy_and_stream::<StoreAccessorMarker>();
        let stats = Rc::new(Mutex::new(StashStats::new()));
        let stats_clone = stats.clone();
        fasync::Task::local(async move {
            let mut stored_value: Option<Value> = None;
            let mut stored_key: Option<String> = None;

            while let Some(req) = stash_stream.try_next().await.unwrap() {
                #[allow(unreachable_patterns)]
                match req {
                    StoreAccessorRequest::GetValue { key, responder } => {
                        stats_clone.lock().await.record(StashAction::Get);
                        if let Some(key_string) = stored_key {
                            assert_eq!(key, key_string);
                        }
                        stored_key = Some(key);

                        let value = stored_value.as_ref().map(|value| match value {
                            Value::Intval(v) => Value::Intval(*v),
                            Value::Floatval(v) => Value::Floatval(*v),
                            Value::Boolval(v) => Value::Boolval(*v),
                            Value::Stringval(v) => Value::Stringval(v.clone()),
                            Value::Bytesval(buffer) => {
                                let opts = zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE;
                                Value::Bytesval(fidl_fuchsia_mem::Buffer {
                                    vmo: buffer.vmo.create_child(opts, 0, buffer.size).unwrap(),
                                    size: buffer.size,
                                })
                            }
                        });
                        responder.send(value).unwrap();
                    }
                    StoreAccessorRequest::SetValue { key, val, control_handle: _ } => {
                        stats_clone.lock().await.record(StashAction::Set);
                        if let Some(key_string) = stored_key {
                            assert_eq!(key, key_string);
                        }
                        stored_key = Some(key);
                        stored_value = Some(val);
                    }
                    StoreAccessorRequest::Flush { responder } => {
                        stats_clone.lock().await.record(StashAction::Flush);
                        let _ = responder.send(Ok(()));
                    }
                    _ => {}
                }
            }
        })
        .detach();
        (stash_proxy, stats)
    }

    const VALUE0: i32 = 3;
    const VALUE1: i32 = 33;

    #[derive(PartialEq, Clone, Serialize, Deserialize, Debug)]
    struct TestStruct {
        value: i32,
    }

    impl DeviceStorageCompatible for TestStruct {
        type Loader = NoneT;
        const KEY: &'static str = "testkey";
    }
    impl Default for TestStruct {
        fn default() -> Self {
            TestStruct { value: VALUE0 }
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_in_memory_storage() {
        let factory = InMemoryStorageFactory::new();
        factory.initialize_storage::<TestStruct>().await;

        let store_1 = factory.get_device_storage().await;
        let store_2 = factory.get_device_storage().await;

        // Write initial data through first store.
        let test_struct = TestStruct { value: VALUE0 };

        // Ensure writing from store1 ends up in store2
        test_write_propagation(store_1.clone(), store_2.clone(), test_struct).await;

        let test_struct_2 = TestStruct { value: VALUE1 };
        // Ensure writing from store2 ends up in store1
        test_write_propagation(store_2.clone(), store_1.clone(), test_struct_2).await;
    }

    async fn test_write_propagation(
        store_1: Rc<DeviceStorage>,
        store_2: Rc<DeviceStorage>,
        data: TestStruct,
    ) {
        assert!(store_1.write(&data).await.is_ok());

        // Ensure it is read in from second store.
        let retrieved_struct = store_2.get().await;
        assert_eq!(data, retrieved_struct);
    }
}
