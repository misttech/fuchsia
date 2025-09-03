// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{SettingInfo, SettingType};
use crate::handler::base::Request;
use crate::handler::setting_handler::persist::{controller as data_controller, ClientProxy};
use crate::handler::setting_handler::{
    controller, ControllerError, IntoHandlerResult, SettingHandlerResult,
};
use crate::privacy::types::PrivacyInfo;
use async_trait::async_trait;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::marker::PhantomData;
use std::rc::Rc;

impl DeviceStorageCompatible for PrivacyInfo {
    type Loader = NoneT;
    const KEY: &'static str = "privacy_info";
}

impl From<PrivacyInfo> for SettingInfo {
    fn from(info: PrivacyInfo) -> SettingInfo {
        SettingInfo::Privacy(info)
    }
}

impl From<&PrivacyInfo> for SettingType {
    fn from(_: &PrivacyInfo) -> SettingType {
        SettingType::Privacy
    }
}

pub struct PrivacyController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for PrivacyController<F> {
    type Storage = DeviceStorage;
    type Data = PrivacyInfo;
    const STORAGE_KEY: &'static str = PrivacyInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for PrivacyController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(PrivacyController { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for PrivacyController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::SetUserDataSharingConsent(user_data_sharing_consent) => {
                let id = fuchsia_trace::Id::new();
                let mut current = self.store.get::<PrivacyInfo>().await;

                // Save the value locally.
                current.user_data_sharing_consent = user_data_sharing_consent;
                Some(
                    self.client.storage_write(&self.store, current, id).await.into_handler_result(),
                )
            }
            Request::Get => Some(Ok(Some(self.store.get::<PrivacyInfo>().await.into()))),
            _ => None,
        }
    }
}
