// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{SettingInfo, SettingType};
use crate::factory_reset::types::FactoryResetInfo;
use crate::handler::base::Request;
use crate::handler::setting_handler::controller::Handle;
use crate::handler::setting_handler::persist::{controller, ClientProxy};
use crate::handler::setting_handler::{
    ControllerError, ControllerStateResult, SettingHandlerResult, State,
};
use crate::service_context::ExternalServiceProxy;
use async_trait::async_trait;
use fidl_fuchsia_recovery_policy::{DeviceMarker, DeviceProxy};
use futures::lock::Mutex;
use settings_common::call;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::marker::PhantomData;
use std::rc::Rc;

impl DeviceStorageCompatible for FactoryResetInfo {
    type Loader = NoneT;
    const KEY: &'static str = "factory_reset_info";
}

impl Default for FactoryResetInfo {
    fn default() -> Self {
        FactoryResetInfo::new(true)
    }
}

impl From<FactoryResetInfo> for SettingInfo {
    fn from(info: FactoryResetInfo) -> SettingInfo {
        SettingInfo::FactoryReset(info)
    }
}

impl From<&FactoryResetInfo> for SettingType {
    fn from(_: &FactoryResetInfo) -> SettingType {
        SettingType::FactoryReset
    }
}

type FactoryResetHandle = Rc<Mutex<FactoryResetManager>>;

/// Handles the mapping between [`Request`]s/[`State`] changes and the
/// [`FactoryResetManager`] logic. Wraps an Rc Mutex of the manager so that each field
/// doesn't need to be individually locked within the manager.
///
/// [`Request`]: crate::handler::base::Request
/// [`State`]: crate::handler::setting_handler::State
pub struct FactoryResetController<F> {
    handle: FactoryResetHandle,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for FactoryResetController<F> {
    type Storage = DeviceStorage;
    type Data = FactoryResetInfo;
    const STORAGE_KEY: &'static str = FactoryResetInfo::KEY;
}

/// Keeps track of the current state of factory reset, is responsible for persisting that state to
/// disk and notifying the fuchsia.recovery.policy.Device fidl interface of any changes.
pub struct FactoryResetManager {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    is_local_reset_allowed: bool,
    factory_reset_policy_service: ExternalServiceProxy<DeviceProxy>,
}

impl FactoryResetManager {
    async fn from_client(
        client: ClientProxy,
        store: Rc<DeviceStorage>,
    ) -> Result<FactoryResetHandle, ControllerError> {
        client
            .get_service_context()
            .connect::<DeviceMarker>()
            .await
            .map(|factory_reset_policy_service| {
                Rc::new(Mutex::new(Self {
                    client,
                    store,
                    is_local_reset_allowed: true,
                    factory_reset_policy_service,
                }))
            })
            .map_err(|_| {
                ControllerError::InitFailure("could not connect to factory reset service".into())
            })
    }

    async fn restore(&mut self) -> SettingHandlerResult {
        self.restore_reset_state(true).await.map(|_| None)
    }

    async fn restore_reset_state(&mut self, send_event: bool) -> ControllerStateResult {
        let info = self.store.get::<FactoryResetInfo>().await;
        self.is_local_reset_allowed = info.is_local_reset_allowed;
        if send_event {
            call!(self.factory_reset_policy_service =>
                set_is_local_reset_allowed(info.is_local_reset_allowed)
            )
            .map_err(|e| {
                ControllerError::ExternalFailure(
                    SettingType::FactoryReset,
                    "factory_reset_policy".into(),
                    "restore_reset_state".into(),
                    format!("{e:?}").into(),
                )
            })?;
        }

        Ok(())
    }

    #[allow(clippy::result_large_err)] // TODO(https://fxbug.dev/42069089)
    fn get(&self) -> SettingHandlerResult {
        Ok(Some(FactoryResetInfo::new(self.is_local_reset_allowed).into()))
    }

    async fn set_local_reset_allowed(
        &mut self,
        is_local_reset_allowed: bool,
    ) -> SettingHandlerResult {
        let id = fuchsia_trace::Id::new();
        let mut info = self.store.get::<FactoryResetInfo>().await;
        self.is_local_reset_allowed = is_local_reset_allowed;
        info.is_local_reset_allowed = is_local_reset_allowed;
        call!(self.factory_reset_policy_service =>
            set_is_local_reset_allowed(info.is_local_reset_allowed)
        )
        .map_err(|e| {
            ControllerError::ExternalFailure(
                SettingType::FactoryReset,
                "factory_reset_policy".into(),
                "set_local_reset_allowed".into(),
                format!("{e:?}").into(),
            )
        })?;
        self.client.storage_write(&self.store, info, id).await.map(|_| None)
    }
}

#[async_trait(?Send)]
impl<F> controller::CreateWithAsync for FactoryResetController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(Self {
            handle: FactoryResetManager::from_client(client, store).await?,
            _phantom: PhantomData,
        })
    }
}

#[async_trait(?Send)]
impl<F> Handle for FactoryResetController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::Restore => Some(self.handle.lock().await.restore().await),
            Request::Get => Some(self.handle.lock().await.get()),
            Request::SetLocalResetAllowed(is_local_reset_allowed) => {
                Some(self.handle.lock().await.set_local_reset_allowed(is_local_reset_allowed).await)
            }
            _ => None,
        }
    }

    async fn change_state(&mut self, state: State) -> Option<ControllerStateResult> {
        match state {
            State::Startup => {
                // Restore the factory reset state locally but do not push to
                // the factory reset policy.
                Some(self.handle.lock().await.restore_reset_state(false).await)
            }
            _ => None,
        }
    }
}
