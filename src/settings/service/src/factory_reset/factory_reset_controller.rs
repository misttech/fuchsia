// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::factory_reset_fidl_handler::Publisher;
use crate::base::{SettingInfo, SettingType};
use crate::factory_reset::types::FactoryResetInfo;
use crate::handler::setting_handler::ControllerError;
use fidl_fuchsia_recovery_policy::{DeviceMarker, DeviceProxy};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot::Sender;
use settings_common::call;
use settings_common::inspect::event::{ExternalEventPublisher, SettingValuePublisher};
use settings_common::service_context::{ExternalServiceProxy, ServiceContext};
use settings_storage::UpdateState;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
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

pub(crate) enum Request {
    Set(FactoryResetInfo, Sender<Result<(), ControllerError>>),
}

pub struct FactoryResetController {
    store: Rc<DeviceStorage>,
    is_local_reset_allowed: bool,
    factory_reset_policy_service: ExternalServiceProxy<DeviceProxy, ExternalEventPublisher>,
    publisher: Option<Publisher>,
    setting_value_publisher: SettingValuePublisher<FactoryResetInfo>,
}

impl StorageAccess for FactoryResetController {
    type Storage = DeviceStorage;
    type Data = FactoryResetInfo;
    const STORAGE_KEY: &'static str = FactoryResetInfo::KEY;
}

impl FactoryResetController {
    pub(crate) async fn new<F>(
        service_context: &ServiceContext,
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<FactoryResetInfo>,
        external_publisher: ExternalEventPublisher,
    ) -> Result<FactoryResetController, ControllerError>
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        let factory_reset_policy_service = service_context
            .connect_with_publisher::<DeviceMarker, _>(external_publisher)
            .await
            .map_err(|e| {
                log::error!("Failed to connect to factory reset service: {e:?}");
                ControllerError::InitFailure("could not connect to factory reset service".into())
            })?;
        Ok(Self {
            store: storage_factory.get_store().await,
            is_local_reset_allowed: true,
            factory_reset_policy_service,
            publisher: None,
            setting_value_publisher,
        })
    }

    pub(crate) fn register_publisher(&mut self, publisher: Publisher) {
        self.publisher = Some(publisher);
    }

    fn publish(&self, info: FactoryResetInfo) {
        let _ = self.setting_value_publisher.publish(&info);
        if let Some(publisher) = self.publisher.as_ref() {
            publisher.set(info);
        }
    }

    pub(crate) async fn handle(
        mut self,
        mut request_rx: UnboundedReceiver<Request>,
    ) -> fasync::Task<()> {
        fasync::Task::local(async move {
            while let Some(request) = request_rx.next().await {
                let Request::Set(info, tx) = request;
                let res =
                    self.set_local_reset_allowed(info.is_local_reset_allowed).await.map(|info| {
                        if let Some(info) = info {
                            self.publish(info);
                        }
                    });
                let _ = tx.send(res);
            }
        })
    }

    pub(crate) async fn restore(&mut self) -> Result<FactoryResetInfo, ControllerError> {
        let info = self.store.get::<FactoryResetInfo>().await;
        self.is_local_reset_allowed = info.is_local_reset_allowed;
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

        Ok(info)
    }

    async fn set_local_reset_allowed(
        &mut self,
        is_local_reset_allowed: bool,
    ) -> Result<Option<FactoryResetInfo>, ControllerError> {
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
        self.store
            .write(&info)
            .await
            .map(|state| (UpdateState::Updated == state).then_some(info))
            .map_err(|e| {
                log::error!("Failed to write factory reset to storage: {e:?}");
                ControllerError::WriteFailure(SettingType::FactoryReset)
            })
    }
}
