// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;
use std::rc::Rc;

use crate::base::{SettingInfo, SettingType};
use crate::handler::base::Request;
use crate::handler::setting_handler::persist::{controller as data_controller, ClientProxy};
use crate::handler::setting_handler::{
    controller, ControllerError, IntoHandlerResult, SettingHandlerResult,
};
use crate::night_mode::types::NightModeInfo;
use async_trait::async_trait;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};

impl DeviceStorageCompatible for NightModeInfo {
    type Loader = NoneT;
    const KEY: &'static str = "night_mode_info";
}

impl From<NightModeInfo> for SettingInfo {
    fn from(info: NightModeInfo) -> SettingInfo {
        SettingInfo::NightMode(info)
    }
}

impl From<&NightModeInfo> for SettingType {
    fn from(_: &NightModeInfo) -> SettingType {
        SettingType::NightMode
    }
}

pub struct NightModeController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for NightModeController<F> {
    type Storage = DeviceStorage;
    type Data = NightModeInfo;
    const STORAGE_KEY: &'static str = NightModeInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for NightModeController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(NightModeController { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for NightModeController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::SetNightModeInfo(night_mode_info) => {
                let id = fuchsia_trace::Id::new();
                let mut current = self.store.get::<NightModeInfo>().await;

                // Save the value locally.
                current.night_mode_enabled = night_mode_info.night_mode_enabled;
                Some(
                    self.client.storage_write(&self.store, current, id).await.into_handler_result(),
                )
            }
            Request::Get => Some(Ok(Some(self.store.get::<NightModeInfo>().await.into()))),
            _ => None,
        }
    }
}
