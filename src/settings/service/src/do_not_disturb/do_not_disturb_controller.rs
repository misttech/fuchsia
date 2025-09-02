// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{SettingInfo, SettingType};
use crate::do_not_disturb::types::DoNotDisturbInfo;
use crate::handler::base::Request;
use crate::handler::setting_handler::persist::{controller as data_controller, ClientProxy};
use crate::handler::setting_handler::{
    controller, ControllerError, IntoHandlerResult, SettingHandlerResult,
};
use async_trait::async_trait;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::marker::PhantomData;
use std::rc::Rc;

impl DeviceStorageCompatible for DoNotDisturbInfo {
    type Loader = NoneT;
    const KEY: &'static str = "do_not_disturb_info";
}

impl Default for DoNotDisturbInfo {
    fn default() -> Self {
        DoNotDisturbInfo::new(false, false)
    }
}

impl From<DoNotDisturbInfo> for SettingInfo {
    fn from(info: DoNotDisturbInfo) -> SettingInfo {
        SettingInfo::DoNotDisturb(info)
    }
}

impl From<&DoNotDisturbInfo> for SettingType {
    fn from(_: &DoNotDisturbInfo) -> SettingType {
        SettingType::DoNotDisturb
    }
}

pub struct DoNotDisturbController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for DoNotDisturbController<F> {
    type Storage = DeviceStorage;
    type Data = DoNotDisturbInfo;
    const STORAGE_KEY: &'static str = DoNotDisturbInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for DoNotDisturbController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(DoNotDisturbController { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for DoNotDisturbController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::SetDnD(dnd_info) => {
                let id = fuchsia_trace::Id::new();
                let mut stored_value = self.store.get::<DoNotDisturbInfo>().await;
                if dnd_info.user_dnd.is_some() {
                    stored_value.user_dnd = dnd_info.user_dnd;
                }
                if dnd_info.night_mode_dnd.is_some() {
                    stored_value.night_mode_dnd = dnd_info.night_mode_dnd;
                }
                Some(
                    self.client
                        .storage_write(&self.store, stored_value, id)
                        .await
                        .into_handler_result(),
                )
            }
            Request::Get => Some(Ok(Some(self.store.get::<DoNotDisturbInfo>().await.into()))),
            _ => None,
        }
    }
}
