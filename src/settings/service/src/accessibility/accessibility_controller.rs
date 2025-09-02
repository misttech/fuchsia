// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::accessibility::types::AccessibilityInfo;
use crate::base::{Merge, SettingInfo, SettingType};
use crate::handler::base::Request;
use crate::handler::setting_handler::persist::{controller as data_controller, ClientProxy};
use crate::handler::setting_handler::{controller, ControllerError, SettingHandlerResult};
use async_trait::async_trait;
use fuchsia_trace as ftrace;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::marker::PhantomData;
use std::rc::Rc;

impl DeviceStorageCompatible for AccessibilityInfo {
    type Loader = NoneT;
    const KEY: &'static str = "accessibility_info";
}

impl From<AccessibilityInfo> for SettingInfo {
    fn from(info: AccessibilityInfo) -> Self {
        SettingInfo::Accessibility(info)
    }
}

impl From<&AccessibilityInfo> for SettingType {
    fn from(_: &AccessibilityInfo) -> Self {
        SettingType::Accessibility
    }
}

pub(crate) struct AccessibilityController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for AccessibilityController<F> {
    type Storage = DeviceStorage;
    type Data = AccessibilityInfo;
    const STORAGE_KEY: &'static str = AccessibilityInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for AccessibilityController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(AccessibilityController { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for AccessibilityController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::Get => Some(Ok(Some(SettingInfo::Accessibility(
                self.store.get::<AccessibilityInfo>().await,
            )))),
            Request::SetAccessibilityInfo(info) => {
                let id = ftrace::Id::new();
                let original_info = self.store.get::<AccessibilityInfo>().await;
                assert!(original_info.is_finite());
                // Validate accessibility info contains valid float numbers.
                if !info.is_finite() {
                    return Some(Err(ControllerError::InvalidArgument(
                        SettingType::Accessibility,
                        "accessibility".into(),
                        format!("{info:?}").into(),
                    )));
                }
                Some(
                    self.client
                        .storage_write(&self.store, original_info.merge(info), id)
                        .await
                        .map(|_| None)
                        .map_err(|e| {
                            log::error!("Failed to write accessibility info: {e:?}");
                            ControllerError::WriteFailure(SettingType::Accessibility)
                        }),
                )
            }
            _ => None,
        }
    }
}
