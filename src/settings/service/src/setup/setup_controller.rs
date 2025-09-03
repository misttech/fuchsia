// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::setup_fidl_handler::InfoPublisher;
use super::types::ConfigurationInterfaceFlags;
use crate::base::{SettingInfo, SettingType};
use crate::handler::setting_handler::ControllerError;
use crate::setup::types::SetupInfo;
use fidl_fuchsia_hardware_power_statecontrol::{RebootOptions, RebootReason2};
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot::Sender;
use futures::StreamExt;
use settings_common::call_async;
use settings_common::inspect::event::{ExternalEventPublisher, SettingValuePublisher};
use settings_common::service_context::ServiceContext;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use settings_storage::UpdateState;
use std::rc::Rc;

async fn reboot(
    service_context: &ServiceContext,
    external_publisher: ExternalEventPublisher,
) -> Result<(), ControllerError> {
    let hardware_power_statecontrol_admin = service_context
        .connect_with_publisher::<fidl_fuchsia_hardware_power_statecontrol::AdminMarker, _>(
            external_publisher,
        )
        .await
        .map_err(|e| {
            ControllerError::ExternalFailure(
                SettingType::Setup,
                "hardware_power_statecontrol_manager".into(),
                "connect".into(),
                format!("{e:?}").into(),
            )
        })?;

    let reboot_err = |e: String| {
        ControllerError::ExternalFailure(
            SettingType::Setup,
            "hardware_power_statecontrol_manager".into(),
            "reboot".into(),
            e.into(),
        )
    };

    call_async!(hardware_power_statecontrol_admin => perform_reboot(&RebootOptions{
        reasons: Some(vec![RebootReason2::UserRequest]), ..Default::default()
    }))
    .await
    .map_err(|e| reboot_err(format!("{e:?}")))
    .and_then(|r| {
        r.map_err(|zx_status| reboot_err(format!("{:?}", zx::Status::from_raw(zx_status))))
    })
}

impl DeviceStorageCompatible for SetupInfo {
    type Loader = NoneT;
    const KEY: &'static str = "setup_info";
}

impl From<SetupInfo> for SettingInfo {
    fn from(info: SetupInfo) -> SettingInfo {
        SettingInfo::Setup(info)
    }
}

impl From<&SetupInfo> for SettingType {
    fn from(_: &SetupInfo) -> SettingType {
        SettingType::Setup
    }
}

pub(crate) enum Request {
    Set(ConfigurationInterfaceFlags, bool, Sender<Result<(), ControllerError>>),
}

pub struct SetupController {
    service_context: Rc<ServiceContext>,
    store: Rc<DeviceStorage>,
    publisher: Option<InfoPublisher>,
    setting_value_publisher: SettingValuePublisher<SetupInfo>,
    external_publisher: ExternalEventPublisher,
}

impl StorageAccess for SetupController {
    type Storage = DeviceStorage;
    type Data = SetupInfo;
    const STORAGE_KEY: &'static str = SetupInfo::KEY;
}

impl SetupController {
    pub(super) async fn new<F>(
        service_context: Rc<ServiceContext>,
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<SetupInfo>,
        external_publisher: ExternalEventPublisher,
    ) -> Self
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        SetupController {
            service_context,
            store: storage_factory.get_store().await,
            publisher: None,
            setting_value_publisher,
            external_publisher,
        }
    }

    pub(super) fn register_publisher(&mut self, publisher: InfoPublisher) {
        self.publisher = Some(publisher);
    }

    fn publish(&self, info: SetupInfo) {
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
                let Request::Set(config_interfaces_flags, should_reboot, tx) = request;
                let res = self.set(config_interfaces_flags, should_reboot).await.map(|info| {
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
        config_interfaces_flags: ConfigurationInterfaceFlags,
        should_reboot: bool,
    ) -> Result<Option<SetupInfo>, ControllerError> {
        let mut info = self.store.get::<SetupInfo>().await;
        info.configuration_interfaces = config_interfaces_flags;

        let write_setting_result = self.store.write(&info).await;

        // If the write succeeded, reboot if necessary.
        if write_setting_result.is_ok() && should_reboot {
            reboot(&self.service_context, self.external_publisher.clone()).await?;
        }
        write_setting_result.map(|state| (UpdateState::Updated == state).then_some(info)).map_err(
            |e| {
                log::error!("Failed to write setup info {e:?}");
                ControllerError::WriteFailure(SettingType::Setup)
            },
        )
    }

    pub(super) async fn restore(&self) -> SetupInfo {
        self.store.get::<SetupInfo>().await
    }
}
