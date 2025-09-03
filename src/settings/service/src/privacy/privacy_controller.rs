// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{SettingInfo, SettingType};
use crate::handler::setting_handler::ControllerError;
use crate::privacy::types::PrivacyInfo;
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot::Sender;
use futures::StreamExt;
use settings_common::inspect::event::SettingValuePublisher;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use settings_storage::UpdateState;
use std::rc::Rc;

use super::privacy_fidl_handler::Publisher;

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

pub(crate) enum Request {
    Set(Option<bool>, Sender<Result<(), ControllerError>>),
}

pub struct PrivacyController {
    store: Rc<DeviceStorage>,
    publisher: Option<Publisher>,
    setting_value_publisher: SettingValuePublisher<PrivacyInfo>,
}

impl PrivacyController {
    pub(super) async fn new<F>(
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<PrivacyInfo>,
    ) -> Self
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        PrivacyController {
            store: storage_factory.get_store().await,
            publisher: None,
            setting_value_publisher,
        }
    }

    pub(super) fn register_publisher(&mut self, publisher: Publisher) {
        self.publisher = Some(publisher);
    }

    fn publish(&self, info: PrivacyInfo) {
        let _ = self.setting_value_publisher.publish(&info);
        if let Some(publisher) = self.publisher.as_ref() {
            publisher.set(info);
        }
    }

    pub(super) async fn handle(
        self,
        mut request_rx: UnboundedReceiver<Request>,
    ) -> fasync::Task<()> {
        fasync::Task::local(async move {
            while let Some(request) = request_rx.next().await {
                let Request::Set(user_data_sharing_consent, tx) = request;
                let res = self.set(user_data_sharing_consent).await.map(|info| {
                    if let Some(info) = info {
                        self.publish(info);
                    }
                });
                let _ = tx.send(res);
            }
        })
    }

    async fn set(
        &self,
        user_data_sharing_consent: Option<bool>,
    ) -> Result<Option<PrivacyInfo>, ControllerError> {
        let mut info = self.store.get::<PrivacyInfo>().await;
        info.user_data_sharing_consent = user_data_sharing_consent;

        self.store
            .write(&info)
            .await
            .map(|state| (UpdateState::Updated == state).then_some(info))
            .map_err(|e| {
                log::error!("Failed to write privacy info: {e:?}");
                ControllerError::WriteFailure(SettingType::Privacy)
            })
    }

    pub(super) async fn restore(&self) -> PrivacyInfo {
        self.store.get::<PrivacyInfo>().await
    }
}

impl StorageAccess for PrivacyController {
    type Storage = DeviceStorage;
    type Data = PrivacyInfo;
    const STORAGE_KEY: &'static str = PrivacyInfo::KEY;
}
