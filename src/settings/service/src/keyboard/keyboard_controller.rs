// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::keyboard_fidl_handler::Publisher;
use crate::base::{SettingInfo, SettingType};
use crate::handler::setting_handler::ControllerError;
use crate::keyboard::types::{KeyboardInfo, KeymapId};
use crate::trace;
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot::Sender;
use settings_common::inspect::event::SettingValuePublisher;
use settings_storage::UpdateState;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::rc::Rc;

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

impl StorageAccess for KeyboardController {
    type Storage = DeviceStorage;
    type Data = KeyboardInfo;
    const STORAGE_KEY: &'static str = KeyboardInfo::KEY;
}

pub(crate) enum Request {
    Set(KeyboardInfo, Sender<Result<(), ControllerError>>),
}

#[cfg(test)]
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Set(info, _) = self;
        f.debug_tuple("Set").field(info).finish_non_exhaustive()
    }
}

pub struct KeyboardController {
    store: Rc<DeviceStorage>,
    publisher: Option<Publisher>,
    setting_value_publisher: SettingValuePublisher<KeyboardInfo>,
}

impl KeyboardController {
    pub(super) async fn new<F>(
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<KeyboardInfo>,
    ) -> Self
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        KeyboardController {
            store: storage_factory.get_store().await,
            publisher: None,
            setting_value_publisher,
        }
    }

    pub(super) fn register_publisher(&mut self, publisher: Publisher) {
        self.publisher = Some(publisher);
    }

    fn publish(&self, info: KeyboardInfo) {
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
                let Request::Set(info, tx) = request;
                let res = self.set(info).await.map(|info| {
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
        keyboard_info: KeyboardInfo,
    ) -> Result<Option<KeyboardInfo>, ControllerError> {
        let id = fuchsia_trace::Id::new();
        trace!(id, c"set keyboard");
        let mut current = self.store.get::<KeyboardInfo>().await;
        if !keyboard_info.is_valid() {
            return Err(ControllerError::InvalidArgument(
                SettingType::Keyboard,
                "keyboard".into(),
                format!("{keyboard_info:?}").into(),
            ));
        }

        current.keymap = keyboard_info.keymap.or(current.keymap);
        current.autorepeat = keyboard_info.autorepeat.or(current.autorepeat).and_then(|value| {
            if value.delay == 0 && value.period == 0 {
                // Clean up Autorepeat when delay and period are set to zero.
                None
            } else {
                Some(value)
            }
        });

        self.store
            .write(&current)
            .await
            .map(|state| (UpdateState::Updated == state).then_some(current))
            .map_err(|e| {
                log::error!("Failed to write keyboard info: {e:?}");
                ControllerError::WriteFailure(SettingType::Keyboard)
            })
    }

    pub(super) async fn restore(&self) -> KeyboardInfo {
        self.store.get::<KeyboardInfo>().await
    }
}
