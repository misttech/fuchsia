// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{SettingInfo, SettingType};
use crate::handler::base::Request;
use crate::handler::setting_handler::persist::{controller as data_controller, ClientProxy};
use crate::handler::setting_handler::{
    controller, ControllerError, IntoHandlerResult, SettingHandlerResult,
};
use crate::service_context::ServiceContext;
use crate::setup::types::SetupInfo;
use async_trait::async_trait;
use fidl_fuchsia_hardware_power_statecontrol::{RebootOptions, RebootReason2};
use std::marker::PhantomData;
use std::rc::Rc;

use settings_common::call_async;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};

async fn reboot(service_context_handle: &ServiceContext) -> Result<(), ControllerError> {
    let hardware_power_statecontrol_admin = service_context_handle
        .connect::<fidl_fuchsia_hardware_power_statecontrol::AdminMarker>()
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

pub struct SetupController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for SetupController<F> {
    type Storage = DeviceStorage;
    type Data = SetupInfo;
    const STORAGE_KEY: &'static str = SetupInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for SetupController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(Self { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for SetupController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::SetConfigurationInterfaces(params) => {
                let id = fuchsia_trace::Id::new();
                let mut info = self.store.get::<SetupInfo>().await;
                info.configuration_interfaces = params.config_interfaces_flags;

                let write_setting_result =
                    self.client.storage_write(&self.store, info, id).await.into_handler_result();

                // If the write succeeded, reboot if necessary.
                if write_setting_result.is_ok() && params.should_reboot {
                    let reboot_result =
                        reboot(&self.client.get_service_context()).await.map(|_| None);
                    if reboot_result.is_err() {
                        // This will result in fidl_fuchsia_settings::Error::Failed in the caller.
                        return Some(reboot_result);
                    }
                }
                Some(write_setting_result)
            }
            Request::Get => Some(Ok(Some(self.store.get::<SetupInfo>().await.into()))),
            _ => None,
        }
    }
}
