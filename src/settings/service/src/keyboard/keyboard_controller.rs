// Copyright 2021 The Fuchsia Authors. All rights reserved.
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
use crate::keyboard::types::{KeyboardInfo, KeymapId};
use crate::trace;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};

use async_trait::async_trait;

impl DeviceStorageCompatible for KeyboardInfo {
    type Loader = NoneT;
    const KEY: &'static str = "keyboard_info";
}

impl Default for KeyboardInfo {
    fn default() -> Self {
        // The US_QWERTY keymap is the default if no settings are ever applied.
        KeyboardInfo { keymap: Some(KeymapId::UsQwerty), autorepeat: None }
    }
}

impl From<KeyboardInfo> for SettingInfo {
    fn from(info: KeyboardInfo) -> SettingInfo {
        SettingInfo::Keyboard(info)
    }
}

impl From<&KeyboardInfo> for SettingType {
    fn from(_: &KeyboardInfo) -> SettingType {
        SettingType::Keyboard
    }
}

pub struct KeyboardController<F> {
    client: ClientProxy,
    store: Rc<DeviceStorage>,
    _phantom: PhantomData<F>,
}

impl<F> StorageAccess for KeyboardController<F> {
    type Storage = DeviceStorage;
    type Data = KeyboardInfo;
    const STORAGE_KEY: &'static str = KeyboardInfo::KEY;
}

#[async_trait(?Send)]
impl<F> data_controller::CreateWithAsync for KeyboardController<F>
where
    F: StorageFactory<Storage = DeviceStorage>,
{
    type Data = Rc<F>;
    async fn create_with(client: ClientProxy, data: Self::Data) -> Result<Self, ControllerError> {
        let store = data.get_store().await;
        Ok(KeyboardController { client, store, _phantom: PhantomData })
    }
}

#[async_trait(?Send)]
impl<F> controller::Handle for KeyboardController<F> {
    async fn handle(&self, request: Request) -> Option<SettingHandlerResult> {
        match request {
            Request::SetKeyboardInfo(keyboard_info) => {
                let id = fuchsia_trace::Id::new();
                trace!(id, c"set keyboard");
                let mut current = self.store.get::<KeyboardInfo>().await;
                if !keyboard_info.is_valid() {
                    return Some(Err(ControllerError::InvalidArgument(
                        SettingType::Keyboard,
                        "keyboard".into(),
                        format!("{keyboard_info:?}").into(),
                    )));
                }
                // Save the value locally.
                current.keymap = keyboard_info.keymap.or(current.keymap);
                current.autorepeat =
                    keyboard_info.autorepeat.or(current.autorepeat).and_then(|value| {
                        if value.delay == 0 && value.period == 0 {
                            // Clean up Autorepeat when delay and period are set to zero.
                            None
                        } else {
                            Some(value)
                        }
                    });
                Some(
                    self.client.storage_write(&self.store, current, id).await.into_handler_result(),
                )
            }
            Request::Get => {
                let id = fuchsia_trace::Id::new();
                trace!(id, c"get keyboard");
                Some(Ok(Some(self.store.get::<KeyboardInfo>().await.into())))
            }
            _ => None,
        }
    }
}
