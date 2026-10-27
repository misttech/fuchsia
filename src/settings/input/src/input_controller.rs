// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device_configuration::InputConfiguration;
use crate::input_fidl_handler::Publisher;
use crate::types::{
    DeviceState, DeviceStateSource, InputDevice, InputDeviceType, InputInfo, InputInfoSources,
    InputState, Microphone,
};
use anyhow::{Context, Error};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot::Sender;
use serde::{Deserialize, Serialize};
use settings_camera::{CameraWatcherEvent, connect_to_cameras};
use settings_common::config::default_settings::DefaultSetting;
use settings_common::inspect::event::{
    ExternalEventPublisher, ResponseType, SettingValuePublisher,
};
use settings_common::service_context::ServiceContext;
use settings_media_buttons::{Event, MediaButtons};
use settings_storage::UpdateState;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{NoneT, StorageAccess, StorageFactory};
use std::borrow::Cow;
use std::rc::Rc;

pub(crate) const DEFAULT_CAMERA_NAME: &str = "camera";
pub(crate) const DEFAULT_MIC_NAME: &str = "microphone";

// The MAX_INPUT_DEVICES, in conjunction with the FIDL API's constraint of 128-byte names, ensures
// that we don't exceed the 64 KiB default transport limit for the API (with some padding). It also
// lessens the chance that we OOM while loading the device list from disk to memory.
pub(crate) const MAX_INPUT_DEVICES: usize = fidl_fuchsia_settings::MAX_INPUT_DEVICES as usize;

type UpdateInputResult = Result<Option<InputInfo>, InputError>;
fn check_publish(
    result: UpdateInputResult,
    publish: impl Fn(InputInfo),
) -> Result<Option<()>, InputError> {
    result.map(|info| info.map(publish))
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum InputError {
    #[error("Failed to initialize controller: {0:?}")]
    InitFailure(Error),
    #[error("Unsupported device type: {0:?}")]
    Unsupported(InputDeviceType),
    #[error("External failure for Input dependency: {0:?} request:{1:?} error:{2}")]
    ExternalFailure(Cow<'static, str>, Cow<'static, str>, Cow<'static, str>),
    #[error("Write failed for Input: {0:?}")]
    WriteFailure(Error),
    #[error("The maximum number of input devices has been reached.")]
    MaximumInputDeviceLimitReached(Cow<'static, str>),
    #[error("Unexpected error: {0}")]
    UnexpectedError(Cow<'static, str>),
}

impl From<&InputError> for ResponseType {
    fn from(error: &InputError) -> Self {
        match error {
            InputError::InitFailure(..) => ResponseType::InitFailure,
            InputError::Unsupported(..) => ResponseType::UnsupportedError,
            InputError::ExternalFailure(..) => ResponseType::ExternalFailure,
            InputError::WriteFailure(..) => ResponseType::StorageFailure,
            InputError::MaximumInputDeviceLimitReached(..) => {
                ResponseType::MaximumInputDevicesReached
            }
            InputError::UnexpectedError(..) => ResponseType::UnexpectedError,
        }
    }
}

impl DeviceStorageCompatible for InputInfoSources {
    type Loader = NoneT;
    const KEY: &'static str = "input_info";

    fn try_deserialize_from(value: &str) -> Result<Self, Error> {
        Self::extract(value).or_else(|e| {
            log::info!("Failed to deserialize InputInfoSources. Falling back to V2: {e:?}");
            InputInfoSourcesV2::try_deserialize_from(value).map(Self::from)
        })
    }
}

impl From<InputInfoSourcesV2> for InputInfoSources {
    fn from(v2: InputInfoSourcesV2) -> Self {
        let mut input_state = v2.input_device_state;

        // Convert the old states into an input device.
        input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::HARDWARE,
            if v2.hw_microphone.muted { DeviceState::MUTED } else { DeviceState::AVAILABLE },
        );
        input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            if v2.sw_microphone.muted { DeviceState::MUTED } else { DeviceState::AVAILABLE },
        );

        InputInfoSources { input_device_state: input_state }
    }
}

impl From<InputInfoSources> for InputInfo {
    fn from(info: InputInfoSources) -> InputInfo {
        InputInfo { input_device_state: info.input_device_state }
    }
}

#[derive(PartialEq, Default, Debug, Clone, Serialize, Deserialize)]
pub struct InputInfoSourcesV2 {
    hw_microphone: Microphone,
    sw_microphone: Microphone,
    input_device_state: InputState,
}

impl DeviceStorageCompatible for InputInfoSourcesV2 {
    type Loader = NoneT;
    const KEY: &'static str = "input_info_sources_v2";

    fn try_deserialize_from(value: &str) -> Result<Self, Error> {
        Self::extract(value).or_else(|e| {
            log::info!("Failed to deserialize InputInfoSourcesV2. Falling back to V1: {e:?}");
            InputInfoSourcesV1::try_deserialize_from(value).map(Self::from)
        })
    }
}

impl From<InputInfoSourcesV1> for InputInfoSourcesV2 {
    fn from(v1: InputInfoSourcesV1) -> Self {
        InputInfoSourcesV2 {
            hw_microphone: v1.hw_microphone,
            sw_microphone: v1.sw_microphone,
            input_device_state: InputState::new(),
        }
    }
}

#[derive(PartialEq, Default, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputInfoSourcesV1 {
    pub hw_microphone: Microphone,
    pub sw_microphone: Microphone,
}

impl DeviceStorageCompatible for InputInfoSourcesV1 {
    type Loader = NoneT;
    const KEY: &'static str = "input_info_sources_v1";
}

pub(crate) enum Request {
    Set(Vec<InputDevice>, Sender<Result<(), InputError>>),
}

pub struct InputController {
    service_context: Rc<ServiceContext>,
    /// Persistent storage.
    store: Rc<DeviceStorage>,

    /// Local tracking of the input device states.
    input_device_state: InputState,

    /// Configuration for this device.
    input_device_config: InputConfiguration,
    publisher: Option<Publisher>,
    setting_value_publisher: SettingValuePublisher<InputInfo>,
    external_publisher: ExternalEventPublisher,
    /// Whether the stored camera software state has been applied to the cameras. Unless
    /// `Synced`, the next `set_input_states` pushes the camera state to all cameras even if it
    /// did not change, since `CameraWatcherAgent` may have updated `input_device_state` in the
    /// interim.
    cam_sw_sync: CamSwSync,
}

/// Whether the stored camera software mute state has been applied to the cameras.
///
/// The camera software state is tracked in three places:
/// - Storage holds the user's preference, which `restore()` applies on startup.
/// - `input_device_state` holds the state reported to clients.
/// - `cam_sw_sync` records how the two relate to the cameras.
///
/// Storage and `input_device_state` agree, except that `input_device_state` may additionally
/// carry `DeviceState::ERROR`, and in `Adopted` (or `Pending` after a camera is added from
/// `Adopted` before its mute state event arrives) it shows the cameras' reported state instead of
/// the stored one.
///
/// A camera event never causes the cameras to be unmuted: the stored state is only pushed from a
/// camera event when it is MUTED and the cameras report unmuted. Because such pushes only ever
/// mute, they cannot make camera events alternate between muted and unmuted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CamSwSync {
    /// The stored state has been applied to all cameras.
    Synced,
    /// The stored state may not be applied to every camera, either because the last push failed or
    /// because a camera was added after startup. A camera event reporting unmuted cameras while
    /// the stored state is MUTED pushes the stored state again.
    Pending,
    /// Pushing the stored MUTED state from a camera event failed. Camera events update the
    /// reported state to match the cameras without pushing again or changing storage, so the
    /// user's preference is still applied on the next startup. The stored state is always MUTED
    /// in this state: it is only entered when the stored state is MUTED, camera events do not
    /// write storage, and `set_input_states` only writes storage after a successful push, which
    /// moves to `Synced`.
    Adopted,
}

impl StorageAccess for InputController {
    type Storage = DeviceStorage;
    type Data = InputInfoSources;
    const STORAGE_KEY: &'static str = InputInfoSources::KEY;
}

impl InputController {
    pub(super) async fn new<F>(
        service_context: Rc<ServiceContext>,
        default_setting: &mut DefaultSetting<InputConfiguration, &'static str>,
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<InputInfo>,
        external_publisher: ExternalEventPublisher,
    ) -> Result<Self, InputError>
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        let input_device_config = default_setting
            .load_default_value()
            .context("Unable to load input device config")
            .map_err(InputError::InitFailure)?
            .expect("Input requires a configuration");
        Ok(InputController::create_with_config(
            service_context,
            input_device_config,
            &*storage_factory,
            setting_value_publisher,
            external_publisher,
        )
        .await)
    }

    /// Alternate constructor that allows specifying a configuration.
    pub(crate) async fn create_with_config<F>(
        service_context: Rc<ServiceContext>,
        input_device_config: InputConfiguration,
        storage_factory: &F,
        setting_value_publisher: SettingValuePublisher<InputInfo>,
        external_publisher: ExternalEventPublisher,
    ) -> Self
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        Self {
            service_context,
            store: storage_factory.get_store().await,
            input_device_state: InputState::new(),
            input_device_config,
            publisher: None,
            setting_value_publisher,
            external_publisher,
            cam_sw_sync: CamSwSync::Synced,
        }
    }

    // Whether the configuration for this device contains a specific |device_type|.
    async fn has_input_device(&self, device_type: InputDeviceType) -> bool {
        let input_device_config_state: InputState = self.input_device_config.clone().into();
        input_device_config_state.device_types().contains(&device_type)
    }

    pub(super) fn register_publisher(&mut self, publisher: Publisher) {
        self.publisher = Some(publisher);
    }

    fn publish(&self, info: InputInfo) {
        let _ = self.setting_value_publisher.publish(&info);
        if let Some(publisher) = self.publisher.as_ref() {
            publisher.set(info);
        }
    }

    pub(super) async fn handle(
        mut self,
        mut camera_event_rx: UnboundedReceiver<(CameraWatcherEvent, super::ResultSender)>,
        mut media_buttons_event_rx: UnboundedReceiver<(Event, super::ResultSender)>,
        mut request_rx: UnboundedReceiver<Request>,
    ) -> fasync::Task<()> {
        fasync::Task::local(async move {
            let mut next_camera_event = camera_event_rx.next();
            let mut next_media_buttons_event = media_buttons_event_rx.next();
            let mut next_request = request_rx.next();
            loop {
                futures::select! {
                    event = next_camera_event => {
                        let Some((event, response_tx)) = event else {
                            continue;
                        };
                        next_camera_event = camera_event_rx.next();
                        let res = match event {
                            CameraWatcherEvent::SoftwareMuted(is_muted) => {
                                self.handle_camera_event(is_muted).await
                            }
                            CameraWatcherEvent::DeviceAdded => Ok(self.handle_camera_added()),
                        };
                        let _ = response_tx.send(res);
                    }
                    event = next_media_buttons_event => {
                        let Some((Event::OnButton(buttons), response_tx)) = event else {
                            continue;
                        };
                        next_media_buttons_event = media_buttons_event_rx.next();
                        let res = self.handle_media_buttons_event(buttons).await;
                        let _ = response_tx.send(res);
                    }
                    request = next_request => {
                        let Some(request) = request else {
                            continue;
                        };
                        next_request = request_rx.next();
                        let Request::Set(input_devices, tx) = request;
                        let res = check_publish(
                            self.set_input_states(input_devices, DeviceStateSource::SOFTWARE).await,
                            |info| self.publish(info)).map(|_|{});
                        let _ = tx.send(res);
                    }
                }
            }
        })
    }

    /// Handles a camera that was connected after startup. The camera driver resets its software
    /// mute state, so the stored state may no longer be applied to every camera. The watcher
    /// reports the cameras' mute state afterwards, which re-applies a stored MUTED state if the
    /// new camera is unmuted.
    fn handle_camera_added(&mut self) -> Option<()> {
        if self.cam_sw_sync != CamSwSync::Pending {
            log::info!("Camera added, re-applying stored camera state on the next camera event");
            self.cam_sw_sync = CamSwSync::Pending;
        }
        None
    }

    async fn handle_camera_event(&mut self, is_muted: bool) -> Result<Option<()>, InputError> {
        let stored_info = self.get_stored_info().await;
        let stored_state = Self::get_cam_sw_state_from(&stored_info.input_device_state)?;
        let stored_muted = stored_state.has_state(DeviceState::MUTED);
        match self.cam_sw_sync {
            CamSwSync::Pending if stored_muted && !is_muted => {
                self.reconcile_cam_sw_state(stored_state, is_muted).await
            }
            CamSwSync::Adopted => {
                if self.show_cam_sw_state(is_muted) {
                    self.publish(InputInfo { input_device_state: self.input_device_state.clone() });
                    Ok(Some(()))
                } else {
                    Ok(None)
                }
            }
            CamSwSync::Synced | CamSwSync::Pending => {
                if stored_muted != is_muted {
                    let res =
                        self.set_sw_camera_mute(is_muted, DEFAULT_CAMERA_NAME.to_string()).await;
                    if res.is_ok() && is_muted {
                        self.cam_sw_sync = CamSwSync::Synced;
                    }
                    if res.is_err() {
                        // The reported state already matches the cameras, so publish it even
                        // though it could not be stored, keeping a cleared ERROR state visible.
                        self.publish(InputInfo {
                            input_device_state: self.input_device_state.clone(),
                        });
                    }
                    check_publish(res, |info| self.publish(info))
                } else {
                    if is_muted {
                        self.cam_sw_sync = CamSwSync::Synced;
                    }
                    if self.show_cam_sw_state(is_muted) {
                        // The cameras report the stored state, so a camera ERROR state is cleared.
                        self.publish(InputInfo {
                            input_device_state: self.input_device_state.clone(),
                        });
                        Ok(Some(()))
                    } else {
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Re-applies the stored MUTED camera software state after the cameras reported being
    /// unmuted while the stored state had not been applied (`CamSwSync::Pending`). If the push
    /// succeeds, the stored state is shown. Otherwise, the cameras' reported state is shown
    /// without changing storage, and the controller moves to `CamSwSync::Adopted`. The reported
    /// state was observed before the push, so it may lag behind the cameras until the next camera
    /// event. In both cases the camera ERROR state is cleared and the result is published.
    async fn reconcile_cam_sw_state(
        &mut self,
        mut stored_state: DeviceState,
        is_muted: bool,
    ) -> Result<Option<()>, InputError> {
        stored_state.remove(DeviceState::ERROR);
        match self.push_cam_sw_state(stored_state).await {
            Ok(()) => {
                self.input_device_state.set_source_state(
                    InputDeviceType::CAMERA,
                    DEFAULT_CAMERA_NAME.to_string(),
                    DeviceStateSource::SOFTWARE,
                    stored_state,
                );
            }
            Err(e) => {
                log::warn!(
                    "Unable to apply stored camera state, reporting camera state \
                    (muted: {is_muted}): {e:?}"
                );
                self.cam_sw_sync = CamSwSync::Adopted;
                let _ = self.show_cam_sw_state(is_muted);
            }
        }
        self.publish(InputInfo { input_device_state: self.input_device_state.clone() });
        Ok(Some(()))
    }

    /// Sets the reported camera software state to `is_muted` without changing storage, clearing
    /// any camera ERROR state. Returns whether the reported state changed.
    fn show_cam_sw_state(&mut self, is_muted: bool) -> bool {
        if self.get_cam_sw_state().is_ok_and(|current| {
            !current.has_state(DeviceState::ERROR)
                && current.has_state(DeviceState::MUTED) == is_muted
        }) {
            return false;
        }
        let state = if is_muted { DeviceState::MUTED } else { DeviceState::AVAILABLE };
        self.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            state,
        );
        true
    }

    async fn handle_media_buttons_event(
        &mut self,
        mut buttons: MediaButtons,
    ) -> Result<Option<()>, InputError> {
        if buttons.mic_mute.is_some() && !self.has_input_device(InputDeviceType::MICROPHONE).await {
            buttons.set_mic_mute(None);
        }
        if buttons.camera_disable.is_some() && !self.has_input_device(InputDeviceType::CAMERA).await
        {
            buttons.set_camera_disable(None);
        }
        check_publish(self.set_hw_media_buttons_state(buttons).await, |info| self.publish(info))
    }

    // Wrapper around client.read() that fills in the config
    // as the default value if the read value is empty. It may be empty
    // after a migration from a previous InputInfoSources version
    // or on pave.
    async fn get_stored_info(&self) -> InputInfo {
        let mut input_info = InputInfo::from(self.store.get::<InputInfo>().await);
        if input_info.input_device_state.is_empty() {
            input_info.input_device_state = self.input_device_config.clone().into();
        }
        input_info
    }

    /// Restores the input state and returns the resulting in-memory state, which reports
    /// `DeviceState::ERROR` for the camera if its software state could not be restored.
    pub(super) async fn restore(&mut self) -> Result<InputInfo, InputError> {
        self.input_device_state = self.get_stored_info().await.input_device_state;

        if self.input_device_config.devices.iter().any(|d| d.device_type == InputDeviceType::CAMERA)
        {
            match self.get_cam_sw_state() {
                Ok(state) => {
                    // Camera setup failure should not prevent start of service. This also allows
                    // clients to see that the camera may not be usable.
                    if let Err(e) = self.push_cam_sw_state(state).await {
                        log::error!("Unable to restore camera state: {e:?}");
                        self.set_cam_err_state(state);
                    }
                }
                Err(e) => {
                    log::error!("Unable to load cam sw state: {e:?}");
                    self.set_cam_err_state(DeviceState::ERROR);
                }
            }
        }
        Ok(InputInfo { input_device_state: self.input_device_state.clone() })
    }

    async fn set_sw_camera_mute(&mut self, disabled: bool, name: String) -> UpdateInputResult {
        let mut input_info = self.get_stored_info().await;
        input_info.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            name.clone(),
            DeviceStateSource::SOFTWARE,
            if disabled { DeviceState::MUTED } else { DeviceState::AVAILABLE },
        );

        self.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            name.clone(),
            DeviceStateSource::SOFTWARE,
            if disabled { DeviceState::MUTED } else { DeviceState::AVAILABLE },
        );
        self.store
            .write(&input_info)
            .await
            .map(|state| (UpdateState::Updated == state).then_some(input_info))
            .context("writing sw camera info")
            .map_err(InputError::WriteFailure)
    }

    /// Sets the hardware mic/cam state from the muted states in `media_buttons`.
    async fn set_hw_media_buttons_state(
        &mut self,
        media_buttons: MediaButtons,
    ) -> UpdateInputResult {
        let mut states_to_process = Vec::new();
        if let Some(mic_mute) = media_buttons.mic_mute {
            states_to_process.push((InputDeviceType::MICROPHONE, mic_mute));
        }
        if let Some(camera_disable) = media_buttons.camera_disable {
            states_to_process.push((InputDeviceType::CAMERA, camera_disable));
        }

        let mut input_info = self.get_stored_info().await;

        for (device_type, muted) in states_to_process.into_iter() {
            // Fetch current state.
            let hw_state_res = input_info.input_device_state.get_source_state(
                device_type,
                device_type.to_string(),
                DeviceStateSource::HARDWARE,
            );

            let mut hw_state = hw_state_res.map_err(|err| {
                InputError::UnexpectedError(
                    format!("Could not fetch current hw mute state: {err:?}").into(),
                )
            })?;

            if muted {
                // Unset available and set muted.
                hw_state &= !DeviceState::AVAILABLE;
                hw_state |= DeviceState::MUTED;
            } else {
                // Set available and unset muted.
                hw_state |= DeviceState::AVAILABLE;
                hw_state &= !DeviceState::MUTED;
            }

            // Set the updated state.
            input_info.input_device_state.set_source_state(
                device_type,
                device_type.to_string(),
                DeviceStateSource::HARDWARE,
                hw_state,
            );
            self.input_device_state.set_source_state(
                device_type,
                device_type.to_string(),
                DeviceStateSource::HARDWARE,
                hw_state,
            );
        }

        // The in-memory state is reported so that a camera ERROR state remains visible.
        self.store
            .write(&input_info)
            .await
            .map(|state| {
                (UpdateState::Updated == state)
                    .then(|| InputInfo { input_device_state: self.input_device_state.clone() })
            })
            .context("writing hw media buttons")
            .map_err(InputError::WriteFailure)
    }

    /// Sets state for the given input devices.
    async fn set_input_states(
        &mut self,
        input_devices: Vec<InputDevice>,
        source: DeviceStateSource,
    ) -> UpdateInputResult {
        let mut input_info = self.get_stored_info().await;
        let device_types = input_info.input_device_state.device_types();
        let cam_state = self.get_cam_sw_state().ok();

        // Firstly, do a validation pass to make sure the input_devices contain only valid devices,
        // and any newly added devices are not going to exceed our maximum device limit.
        let mut new_devices = Vec::new();
        for input_device in input_devices.iter() {
            if !device_types.contains(&input_device.device_type) {
                return Err(InputError::Unsupported(input_device.device_type));
            }

            let already_exists = input_info
                .input_device_state
                .contains_device(input_device.device_type, &input_device.name);

            let already_counted = new_devices
                .iter()
                .any(|(dt, name)| *dt == input_device.device_type && *name == &input_device.name);

            if !already_exists && !already_counted {
                new_devices.push((input_device.device_type, &input_device.name));
            }
        }

        // Abort if applying these new devices would exceed the limit.
        if input_info.input_device_state.total_devices() + new_devices.len() > MAX_INPUT_DEVICES {
            log::error!(
                "Maximum number of supported input devices ({MAX_INPUT_DEVICES}) has been reached."
            );
            return Err(InputError::MaximumInputDeviceLimitReached(
                format!("Maximum limit of {MAX_INPUT_DEVICES} input devices has been reached.")
                    .into(),
            ));
        }

        // Update the function-scoped storage cache with the requested changes. This will allow us
        // to determine if any camera state has changed, necessary for the logic which follows.
        for input_device in &input_devices {
            input_info.input_device_state.insert_device(input_device.clone(), source);
        }

        // If the device has a camera, it should successfully get the sw state, and
        // push the state if it has changed. If the device does not have a camera,
        // it should be None both here and above, and thus not detect a change.
        //
        // If the stored camera state has not been applied to every camera, it is pushed again.
        // When the request does not include the camera, that push only happens for a MUTED state
        // and is best-effort, so a camera failure neither unmutes a camera nor blocks other
        // changes such as muting the microphone.
        //
        // Once a push succeeds, the reported camera state matches the pushed state. If that
        // changes the reported state (for example, clearing a camera ERROR state), the result is
        // reported even if storage is unchanged so that watchers see the change.
        let modified_cam_state = Self::get_cam_sw_state_from(&input_info.input_device_state).ok();
        let camera_requested =
            input_devices.iter().any(|device| device.device_type == InputDeviceType::CAMERA);
        let push_cam_state = if camera_requested {
            cam_state != modified_cam_state || self.cam_sw_sync != CamSwSync::Synced
        } else {
            self.cam_sw_sync != CamSwSync::Synced
                && modified_cam_state.is_some_and(|state| state.has_state(DeviceState::MUTED))
        };
        let mut reported_cam_state_changed = false;
        if push_cam_state && let Some(state) = modified_cam_state {
            let pushed = match self.push_cam_sw_state(state).await {
                Ok(()) => true,
                Err(e) if !camera_requested => {
                    log::warn!("Unable to re-apply stored camera state: {e:?}");
                    false
                }
                Err(e) => return Err(e),
            };
            if pushed && cam_state != Some(state) {
                self.input_device_state.set_source_state(
                    InputDeviceType::CAMERA,
                    DEFAULT_CAMERA_NAME.to_string(),
                    DeviceStateSource::SOFTWARE,
                    state,
                );
                reported_cam_state_changed = true;
            }
        }

        // All prerequisites are done, and camera changes are committed to the hardware. We can now
        // finalize the update by updating our cache, and queue the write to storage.
        for input_device in input_devices {
            self.input_device_state.insert_device(input_device, source);
        }

        let published_info = InputInfo { input_device_state: self.input_device_state.clone() };
        self.store
            .write(&input_info)
            .await
            .map(|state| {
                (UpdateState::Updated == state || reported_cam_state_changed)
                    .then_some(published_info)
            })
            .context("writing input states")
            .map_err(InputError::WriteFailure)
    }

    /// Pulls the software state of the camera from the given `input_state`.
    fn get_cam_sw_state_from(input_state: &InputState) -> Result<DeviceState, InputError> {
        input_state
            .get_source_state(
                InputDeviceType::CAMERA,
                DEFAULT_CAMERA_NAME.to_string(),
                DeviceStateSource::SOFTWARE,
            )
            .map_err(|e| {
                InputError::UnexpectedError(
                    format!("Could not find camera software state: {e:?}").into(),
                )
            })
    }

    /// Pulls the current software state of the camera from the device state.
    fn get_cam_sw_state(&self) -> Result<DeviceState, InputError> {
        Self::get_cam_sw_state_from(&self.input_device_state)
    }

    /// Set the camera state into an error condition.
    fn set_cam_err_state(&mut self, mut state: DeviceState) {
        state.set(DeviceState::ERROR, true);
        self.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            state,
        )
    }

    /// Forwards the given software state to the camera3 api. Will first establish
    /// a connection to the camera3.DeviceWatcher api. This function should only be called
    /// when there is a camera included in the config. The config is used to populate the
    /// stored input_info, so the input_info's input_device_state can be checked whether its
    /// device_types contains Camera prior to calling this function.
    ///
    /// Multiple cameras are not yet supported individually, and individual camera mute/unmute is
    /// not exposed, so the software mute state is applied across all reported camera devices.
    async fn push_cam_sw_state(&mut self, cam_state: DeviceState) -> Result<(), InputError> {
        let result = self.try_push_cam_sw_state(cam_state).await;
        self.cam_sw_sync = match (&result, self.cam_sw_sync) {
            (Ok(()), _) => CamSwSync::Synced,
            (Err(_), CamSwSync::Synced) => CamSwSync::Pending,
            (Err(_), sync) => sync,
        };
        result
    }

    /// Sends the software mute state in `cam_state` to all reported camera devices, returning an
    /// error if any of them could not be updated.
    async fn try_push_cam_sw_state(&self, cam_state: DeviceState) -> Result<(), InputError> {
        let is_muted = cam_state.has_state(DeviceState::MUTED);

        // Start up a connection to the camera device watcher and connect to the
        // camera proxies using the ids that are returned. The connections will drop out
        // of scope after the mute state is sent.
        let camera_proxies =
            connect_to_cameras(&self.service_context, self.external_publisher.clone())
                .await
                .map_err(|e| {
                    InputError::UnexpectedError(
                        format!("Could not connect to camera device: {e:?}").into(),
                    )
                })?;

        let results = futures::future::join_all(
            camera_proxies.iter().map(|proxy| proxy.set_software_mute_state(is_muted)),
        )
        .await;

        let mut mute_error = None;
        for result in results {
            if let Err(e) = result {
                log::warn!("Failed to set software mute state on camera: {e:?}");
                mute_error = Some(InputError::ExternalFailure(
                    "fuchsia.camera3.Device".into(),
                    "SetSoftwareMuteState".into(),
                    format!("{e:?}").into(),
                ));
            }
        }
        if let Some(err) = mute_error {
            return Err(err);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_device_configuration::{InputDeviceConfiguration, SourceState};
    use fuchsia_async as fasync;
    use fuchsia_inspect::component;
    use futures::channel::mpsc;
    use settings_common::inspect::config_logger::InspectConfigLogger;
    use settings_common::service_context::ServiceContext;
    use settings_test_common::storage::InMemoryStorageFactory;

    #[fuchsia::test]
    fn test_input_migration_v1_to_current() {
        const MUTED_MIC: Microphone = Microphone { muted: true };
        let v1 = InputInfoSourcesV1 { sw_microphone: MUTED_MIC, ..Default::default() };

        let serialized_v1 = v1.serialize_to();
        let current = InputInfoSources::try_deserialize_from(&serialized_v1)
            .expect("deserialization should succeed");
        let mut expected_input_state = InputState::new();
        expected_input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            DeviceState::MUTED,
        );
        expected_input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::HARDWARE,
            DeviceState::AVAILABLE,
        );
        assert_eq!(current.input_device_state, expected_input_state);
    }

    #[fuchsia::test]
    fn test_input_migration_v1_to_v2() {
        const MUTED_MIC: Microphone = Microphone { muted: true };
        let v1 = InputInfoSourcesV1 { sw_microphone: MUTED_MIC, ..Default::default() };

        let serialized_v1 = v1.serialize_to();
        let v2 = InputInfoSourcesV2::try_deserialize_from(&serialized_v1)
            .expect("deserialization should succeed");

        assert_eq!(v2.hw_microphone, Microphone { muted: false });
        assert_eq!(v2.sw_microphone, MUTED_MIC);
        assert_eq!(v2.input_device_state, InputState::new());
    }

    #[fuchsia::test]
    fn test_input_migration_v2_to_current() {
        const DEFAULT_CAMERA_NAME: &str = "camera";
        const MUTED_MIC: Microphone = Microphone { muted: true };
        let mut v2 = InputInfoSourcesV2::default();
        v2.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            DeviceState::AVAILABLE,
        );
        v2.input_device_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::HARDWARE,
            DeviceState::MUTED,
        );
        v2.sw_microphone = MUTED_MIC;

        let serialized_v2 = v2.serialize_to();
        let current = InputInfoSources::try_deserialize_from(&serialized_v2)
            .expect("deserialization should succeed");
        let mut expected_input_state = InputState::new();

        expected_input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            DeviceState::MUTED,
        );
        expected_input_state.set_source_state(
            InputDeviceType::MICROPHONE,
            DEFAULT_MIC_NAME.to_string(),
            DeviceStateSource::HARDWARE,
            DeviceState::AVAILABLE,
        );
        expected_input_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::SOFTWARE,
            DeviceState::AVAILABLE,
        );
        expected_input_state.set_source_state(
            InputDeviceType::CAMERA,
            DEFAULT_CAMERA_NAME.to_string(),
            DeviceStateSource::HARDWARE,
            DeviceState::MUTED,
        );

        assert_eq!(current.input_device_state, expected_input_state);
    }

    #[fuchsia::test]
    async fn test_camera_error_on_restore() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, _value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);
        let mut controller: InputController =
            InputController::create_with_config::<InMemoryStorageFactory>(
                Rc::new(ServiceContext::new(None)),
                InputConfiguration {
                    devices: vec![InputDeviceConfiguration {
                        device_name: DEFAULT_CAMERA_NAME.to_string(),
                        device_type: InputDeviceType::CAMERA,
                        source_states: vec![SourceState {
                            source: DeviceStateSource::SOFTWARE,
                            state: 0,
                        }],
                        mutable_toggle_state: 0,
                    }],
                },
                &storage_factory,
                setting_value_publisher,
                external_publisher,
            )
            .await;

        // Restore should pass.
        let result = controller.restore().await;
        assert!(result.is_ok());

        // But the camera state should show an error.
        let camera_state = controller
            .input_device_state
            .get_state(InputDeviceType::CAMERA, DEFAULT_CAMERA_NAME.to_string())
            .unwrap();
        assert!(camera_state.has_state(DeviceState::ERROR));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_controller_creation_with_default_config() {
        let config_logger = InspectConfigLogger::new(component::inspector().root());
        let mut default_setting = DefaultSetting::new(
            Some(InputConfiguration::default()),
            "/config/data/input_device_config.json",
            Rc::new(std::sync::Mutex::new(config_logger)),
        );

        let (event_tx, _) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, _value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);
        let _controller = InputController::new(
            Rc::new(ServiceContext::new(None)),
            &mut default_setting,
            Rc::new(storage_factory),
            setting_value_publisher,
            external_publisher,
        )
        .await
        .expect("Should have controller");
    }

    #[fuchsia::test]
    async fn test_set_input_states_limit() {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, _value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);

        let mut device_configs = Vec::new();
        for i in 0..MAX_INPUT_DEVICES {
            device_configs.push(InputDeviceConfiguration {
                device_name: format!("mic{i}"),
                device_type: InputDeviceType::MICROPHONE,
                source_states: vec![SourceState { source: DeviceStateSource::SOFTWARE, state: 0 }],
                mutable_toggle_state: 0,
            });
        }

        let mut controller: InputController =
            InputController::create_with_config::<InMemoryStorageFactory>(
                Rc::new(ServiceContext::new(None)),
                InputConfiguration { devices: device_configs },
                &storage_factory,
                setting_value_publisher,
                external_publisher,
            )
            .await;

        let _ = controller.restore().await;

        let overflow_dev = InputDevice {
            name: "mic_max_exceeded".to_string(),
            device_type: InputDeviceType::MICROPHONE,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::AVAILABLE)].into(),
            state: DeviceState::AVAILABLE,
        };
        let res =
            controller.set_input_states(vec![overflow_dev], DeviceStateSource::SOFTWARE).await;
        match res {
            Err(InputError::MaximumInputDeviceLimitReached(msg)) => {
                assert_eq!(
                    msg,
                    format!("Maximum limit of {MAX_INPUT_DEVICES} input devices has been reached.")
                );
            }
            _ => panic!("Expected MaximumInputDeviceLimitReached, got {res:?}"),
        }
    }

    async fn handle_camera3_device(
        mut stream: fidl_fuchsia_camera3::DeviceRequestStream,
        camera_muted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use futures::StreamExt;
        use std::sync::atomic::Ordering;
        while let Some(Ok(req)) = stream.next().await {
            if let fidl_fuchsia_camera3::DeviceRequest::SetSoftwareMuteState { muted, responder } =
                req
            {
                camera_muted.store(muted, Ordering::Relaxed);
                let _ = responder.send();
            }
        }
    }

    async fn handle_camera3_device_watcher(
        mut stream: fidl_fuchsia_camera3::DeviceWatcherRequestStream,
        camera_muted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use futures::StreamExt;
        while let Some(Ok(req)) = stream.next().await {
            #[allow(unreachable_patterns)]
            match req {
                fidl_fuchsia_camera3::DeviceWatcherRequest::WatchDevices { responder } => {
                    let _ = responder.send(&[fidl_fuchsia_camera3::WatchDevicesEvent::Added(1)]);
                }
                fidl_fuchsia_camera3::DeviceWatcherRequest::ConnectToDevice {
                    id: _,
                    request,
                    control_handle: _,
                } => {
                    let camera_muted = camera_muted.clone();
                    fasync::Task::local(handle_camera3_device(request.into_stream(), camera_muted))
                        .detach();
                }
                _ => {}
            }
        }
    }

    #[fuchsia::test]
    // This tests the bug documented in https://fxbug.dev/521503847. This test validates that
    // the in-memory cached state of the Settings service doesn't commit changes prior to
    // updating the hardware state. If that were to happen, the camera hardware could get "stuck"
    // in its current state, while further updates are rejected due to the internal cache believing
    // that the update is already applied.
    async fn test_set_input_states_retry_on_failure() {
        use fidl::endpoints::DiscoverableProtocolMarker;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let camera_muted = Arc::new(AtomicBool::new(false));
        let camera_muted_clone = camera_muted.clone();

        // Mock the camera service, rejecting the first call (count == 1) to simulate a failure.
        let service_context = Rc::new(ServiceContext::new(Some(Box::new(
            move |service_name: &str, channel: zx::Channel| {
                let count = call_count_clone.fetch_add(1, Ordering::Relaxed);
                let camera_muted_clone = camera_muted_clone.clone();
                let service_name = service_name.to_string();
                Box::pin(async move {
                    if count == 1 {
                        return Err(anyhow::Error::msg("injected failure"));
                    }
                    if service_name != fidl_fuchsia_camera3::DeviceWatcherMarker::PROTOCOL_NAME {
                        return Err(anyhow::Error::msg("unsupported service"));
                    }
                    let stream = fidl::endpoints::ServerEnd::<
                        fidl_fuchsia_camera3::DeviceWatcherMarker,
                    >::new(channel)
                    .into_stream();
                    fasync::Task::local(handle_camera3_device_watcher(stream, camera_muted_clone))
                        .detach();
                    Ok(())
                })
            },
        ))));

        // Initialize the mocked "persistent" storage.
        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, _value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        let mut controller = InputController::create_with_config::<InMemoryStorageFactory>(
            service_context,
            InputConfiguration {
                devices: vec![InputDeviceConfiguration {
                    device_name: DEFAULT_CAMERA_NAME.to_string(),
                    device_type: InputDeviceType::CAMERA,
                    source_states: vec![SourceState {
                        source: DeviceStateSource::SOFTWARE,
                        state: 0, // AVAILABLE
                    }],
                    mutable_toggle_state: 0,
                }],
            },
            &storage_factory,
            setting_value_publisher,
            external_publisher,
        )
        .await;

        let _ = controller.restore().await;

        // Verify initial state is AVAILABLE (not muted).
        let camera_state = controller
            .input_device_state
            .get_state(InputDeviceType::CAMERA, DEFAULT_CAMERA_NAME.to_string())
            .unwrap();
        assert!(!camera_state.has_state(DeviceState::MUTED));

        // Create a request to MUTE the camera. The hardware API call will fail on this request,
        // due to the previously configured mock camera service.
        let mute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };

        let res = controller
            .set_input_states(vec![mute_device.clone()], DeviceStateSource::SOFTWARE)
            .await;
        assert!(res.is_err());
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
        assert!(!camera_muted.load(Ordering::Relaxed));

        // Attempt to mute again. If the bug this test tracks is present, this will skip
        // push_cam_sw_state and return Ok, because the service thinks the state is already MUTED
        // due to the in-memory state being updated despite the hardware update failing.
        let res2 =
            controller.set_input_states(vec![mute_device], DeviceStateSource::SOFTWARE).await;

        // If the bug is fixed, we expect that the camera service will have been called again, the
        // result was okay and the mute state was properly updated.
        assert_eq!(call_count.load(Ordering::Relaxed), 3);
        assert!(res2.is_ok());
        assert!(camera_muted.load(Ordering::Relaxed));
    }

    async fn setup_multi_camera_test(
        initial_muted: bool,
        fail_camera_1_on_attempt: Option<usize>,
    ) -> (
        InputController,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(
                initial_muted,
                fail_camera_1_on_attempt.into_iter().collect(),
            )
            .await;
        (controller, camera1_muted, camera2_muted)
    }

    /// Creates a controller with two cameras and runs `restore()`. Connecting to camera 1 fails on
    /// each 0-based attempt listed in `fail_camera_1_on_attempts`, where attempt 0 is the push made
    /// by `restore()`. Also returns the receiver for values published by the controller.
    async fn setup_multi_camera_test_with_failures(
        initial_muted: bool,
        fail_camera_1_on_attempts: Vec<usize>,
    ) -> (
        InputController,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        mpsc::UnboundedReceiver<(&'static str, String)>,
    ) {
        use fidl::endpoints::DiscoverableProtocolMarker;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let camera1_muted = Arc::new(AtomicBool::new(initial_muted));
        let camera2_muted = Arc::new(AtomicBool::new(initial_muted));
        let camera1_clone = camera1_muted.clone();
        let camera2_clone = camera2_muted.clone();
        let camera1_attempts = Arc::new(AtomicUsize::new(0));

        let service_context = Rc::new(ServiceContext::new(Some(Box::new(
            move |service_name: &str, channel: zx::Channel| {
                let camera1 = camera1_clone.clone();
                let camera2 = camera2_clone.clone();
                let camera1_attempts = camera1_attempts.clone();
                let fail_camera_1_on_attempts = fail_camera_1_on_attempts.clone();
                let service_name = service_name.to_string();
                Box::pin(async move {
                    if service_name != fidl_fuchsia_camera3::DeviceWatcherMarker::PROTOCOL_NAME {
                        return Err(anyhow::Error::msg("unsupported service"));
                    }
                    let mut stream = fidl::endpoints::ServerEnd::<
                        fidl_fuchsia_camera3::DeviceWatcherMarker,
                    >::new(channel)
                    .into_stream();
                    fasync::Task::local(async move {
                        while let Some(Ok(req)) = stream.next().await {
                            #[allow(unreachable_patterns)]
                            match req {
                                fidl_fuchsia_camera3::DeviceWatcherRequest::WatchDevices {
                                    responder,
                                } => {
                                    let _ = responder.send(&[
                                        fidl_fuchsia_camera3::WatchDevicesEvent::Existing(1),
                                        fidl_fuchsia_camera3::WatchDevicesEvent::Added(2),
                                    ]);
                                }
                                fidl_fuchsia_camera3::DeviceWatcherRequest::ConnectToDevice {
                                    id,
                                    request,
                                    control_handle: _,
                                } => {
                                    let target = match id {
                                        1 => {
                                            let attempt =
                                                camera1_attempts.fetch_add(1, Ordering::Relaxed);
                                            if fail_camera_1_on_attempts.contains(&attempt) {
                                                drop(request);
                                                continue;
                                            }
                                            camera1.clone()
                                        }
                                        2 => camera2.clone(),
                                        _ => panic!("unexpected camera id {id}"),
                                    };
                                    fasync::Task::local(handle_camera3_device(
                                        request.into_stream(),
                                        target,
                                    ))
                                    .detach();
                                }
                                _ => {}
                            }
                        }
                    })
                    .detach();
                    Ok(())
                })
            },
        ))));

        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        let initial_state = if initial_muted {
            DeviceState::MUTED.bits()
        } else {
            0 // AVAILABLE
        };

        let mut controller = InputController::create_with_config::<InMemoryStorageFactory>(
            service_context,
            InputConfiguration {
                devices: vec![
                    InputDeviceConfiguration {
                        device_name: DEFAULT_CAMERA_NAME.to_string(),
                        device_type: InputDeviceType::CAMERA,
                        source_states: vec![SourceState {
                            source: DeviceStateSource::SOFTWARE,
                            state: initial_state,
                        }],
                        mutable_toggle_state: 0,
                    },
                    InputDeviceConfiguration {
                        device_name: DEFAULT_MIC_NAME.to_string(),
                        device_type: InputDeviceType::MICROPHONE,
                        source_states: vec![SourceState {
                            source: DeviceStateSource::SOFTWARE,
                            state: 0, // AVAILABLE
                        }],
                        mutable_toggle_state: 0,
                    },
                ],
            },
            &storage_factory,
            setting_value_publisher,
            external_publisher,
        )
        .await;

        let _ = controller.restore().await;
        (controller, camera1_muted, camera2_muted, value_rx)
    }

    #[fuchsia::test]
    async fn test_set_input_states_multiple_cameras() {
        use std::sync::atomic::Ordering;

        let (mut controller, camera1_muted, camera2_muted) =
            setup_multi_camera_test(false, None).await;
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert!(!camera2_muted.load(Ordering::Relaxed));

        let mute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };

        let res = controller.set_input_states(vec![mute_device], DeviceStateSource::SOFTWARE).await;
        assert!(res.is_ok());
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn test_set_input_states_multiple_cameras_partial_failure() {
        use std::sync::atomic::Ordering;

        // Fail camera 1 on attempt 1 (the first set_input_states call after restore()).
        let (mut controller, camera1_muted, camera2_muted) =
            setup_multi_camera_test(false, Some(1)).await;

        let mute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };

        let res = controller.set_input_states(vec![mute_device], DeviceStateSource::SOFTWARE).await;
        assert!(res.is_err());
        assert!(!camera1_muted.load(Ordering::Relaxed));
        // Even though camera 1 failed, camera 2 must still have been muted.
        assert!(camera2_muted.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn test_set_input_states_multiple_cameras_partial_unmute_retry() {
        use std::sync::atomic::Ordering;

        // Both cameras start muted. Fail camera 1 on attempt 1 (the first unmute attempt).
        let (mut controller, camera1_muted, camera2_muted) =
            setup_multi_camera_test(true, Some(1)).await;
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));

        let unmute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::AVAILABLE)].into(),
            state: DeviceState::AVAILABLE,
        };

        // First unmute attempt fails on camera 1 while camera 2 unmutes.
        let res = controller
            .set_input_states(vec![unmute_device.clone()], DeviceStateSource::SOFTWARE)
            .await;
        assert!(res.is_err());
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(!camera2_muted.load(Ordering::Relaxed));

        // Simulate CameraWatcherAgent reporting that the aggregate mute state is now unmuted
        // (`false`) because camera 2 unmuted. The stored MUTED state has not been fully applied,
        // so it is pushed to both cameras again.
        let watcher_res = controller.handle_camera_event(false).await;
        assert!(watcher_res.is_ok());
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));

        // Retrying set_input_states(AVAILABLE) pushes SetSoftwareMuteState(false) to both cameras.
        let retry_res =
            controller.set_input_states(vec![unmute_device], DeviceStateSource::SOFTWARE).await;
        assert!(retry_res.is_ok());
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert!(!camera2_muted.load(Ordering::Relaxed));
    }

    async fn create_camera_controller_without_camera_service(
        source_states: Vec<SourceState>,
    ) -> InputController {
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);
        let storage_factory = InMemoryStorageFactory::new();
        storage_factory
            .initialize::<InputController>()
            .await
            .expect("controller should have impls");
        let (value_tx, _value_rx) = mpsc::unbounded();
        let setting_value_publisher = SettingValuePublisher::new(value_tx);
        InputController::create_with_config::<InMemoryStorageFactory>(
            Rc::new(ServiceContext::new(None)),
            InputConfiguration {
                devices: vec![InputDeviceConfiguration {
                    device_name: DEFAULT_CAMERA_NAME.to_string(),
                    device_type: InputDeviceType::CAMERA,
                    source_states,
                    mutable_toggle_state: 0,
                }],
            },
            &storage_factory,
            setting_value_publisher,
            external_publisher,
        )
        .await
    }

    #[fuchsia::test]
    async fn test_restore_returns_camera_error_on_push_failure() {
        let mut controller = create_camera_controller_without_camera_service(vec![SourceState {
            source: DeviceStateSource::SOFTWARE,
            state: DeviceState::MUTED.bits(),
        }])
        .await;

        let info = controller.restore().await.expect("restore should succeed");

        let camera_state = info
            .input_device_state
            .get_state(InputDeviceType::CAMERA, DEFAULT_CAMERA_NAME.to_string())
            .unwrap();
        assert!(camera_state.has_state(DeviceState::ERROR));
        let sw_state = InputController::get_cam_sw_state_from(&info.input_device_state).unwrap();
        assert_eq!(sw_state, DeviceState::MUTED | DeviceState::ERROR);
        assert_eq!(info.input_device_state, controller.input_device_state);
    }

    #[fuchsia::test]
    async fn test_restore_returns_camera_error_on_missing_sw_state() {
        let mut controller = create_camera_controller_without_camera_service(vec![SourceState {
            source: DeviceStateSource::HARDWARE,
            state: DeviceState::AVAILABLE.bits(),
        }])
        .await;

        let info = controller.restore().await.expect("restore should succeed");

        let sw_state = InputController::get_cam_sw_state_from(&info.input_device_state).unwrap();
        assert_eq!(sw_state, DeviceState::ERROR);
        assert_eq!(info.input_device_state, controller.input_device_state);
    }

    #[fuchsia::test]
    async fn test_media_buttons_keep_camera_error() {
        let mut controller = create_camera_controller_without_camera_service(vec![
            SourceState {
                source: DeviceStateSource::HARDWARE,
                state: DeviceState::AVAILABLE.bits(),
            },
            SourceState { source: DeviceStateSource::SOFTWARE, state: DeviceState::MUTED.bits() },
        ])
        .await;
        let _ = controller.restore().await.expect("restore should succeed");

        // A hardware camera button press while the camera software state could not be restored
        // reports the new hardware state together with the camera ERROR state.
        let info = controller
            .set_hw_media_buttons_state(MediaButtons { mic_mute: None, camera_disable: Some(true) })
            .await
            .expect("media buttons should succeed")
            .expect("hardware state should be published");
        assert_eq!(
            InputController::get_cam_sw_state_from(&info.input_device_state).unwrap(),
            DeviceState::MUTED | DeviceState::ERROR
        );
        assert_eq!(
            info.input_device_state
                .get_source_state(
                    InputDeviceType::CAMERA,
                    DEFAULT_CAMERA_NAME.to_string(),
                    DeviceStateSource::HARDWARE
                )
                .unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(info.input_device_state, controller.input_device_state);
    }

    fn expected_published_value(controller: &InputController) -> String {
        format!("{:?}", InputInfo { input_device_state: controller.input_device_state.clone() })
    }

    #[fuchsia::test]
    async fn test_camera_event_while_pending_repush_succeeds() {
        use std::sync::atomic::Ordering;

        // The stored state is MUTED, and the push made by restore() fails on camera 1, which is
        // left unmuted.
        let (mut controller, camera1_muted, camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(true, vec![0]).await;
        camera1_muted.store(false, Ordering::Relaxed);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);
        assert!(
            InputController::get_cam_sw_state_from(&controller.input_device_state)
                .unwrap()
                .has_state(DeviceState::ERROR)
        );

        // The watcher reports the cameras as unmuted.
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");

        // The stored MUTED state is pushed to the cameras again and kept.
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );

        // The reconciled state is published even though storage did not change.
        let (_, published) = value_rx.try_recv().expect("reconciled state should be published");
        assert_eq!(published, expected_published_value(&controller));
        assert!(value_rx.try_recv().is_err());
    }

    #[fuchsia::test]
    async fn test_camera_event_while_pending_repush_fails() {
        use std::sync::atomic::Ordering;

        // The stored state is MUTED. The push made by restore() and the following re-push both
        // fail on camera 1, which is left unmuted.
        let (mut controller, camera1_muted, camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(true, vec![0, 1]).await;
        camera1_muted.store(false, Ordering::Relaxed);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);

        // The watcher reports the cameras as unmuted.
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);

        // The cameras' reported state is shown without ERROR, and the stored MUTED preference is
        // kept so that it is applied again on the next startup.
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::AVAILABLE
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );

        let (_, published) = value_rx.try_recv().expect("reported state should be published");
        assert_eq!(published, expected_published_value(&controller));
        assert!(value_rx.try_recv().is_err());

        // Another client mutes camera 1, so the watcher reports the cameras as muted. Later camera
        // events update the reported state without pushing or changing storage.
        camera1_muted.store(true, Ordering::Relaxed);
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(None)), "unexpected result {res:?}");

        // Another client unmutes camera 1. The event is reported without pushing MUTED again.
        camera1_muted.store(false, Ordering::Relaxed);
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
    }

    #[fuchsia::test]
    async fn test_camera_event_while_pending_never_unmutes() {
        use std::sync::atomic::Ordering;

        // The stored state is AVAILABLE, and the push made by restore() fails on camera 1.
        let (mut controller, camera1_muted, camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(false, vec![0]).await;
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);

        // Another client mutes both cameras and the watcher reports the cameras as muted.
        camera1_muted.store(true, Ordering::Relaxed);
        camera2_muted.store(true, Ordering::Relaxed);
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");

        // The cameras are not unmuted, and their muted state is adopted.
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
        assert!(value_rx.try_recv().is_ok());
    }

    #[fuchsia::test]
    async fn test_camera_event_while_pending_matching_storage_clears_error() {
        // The push made by restore() fails on camera 1.
        let (mut controller, _camera1_muted, _camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(true, vec![0]).await;
        let stored_info = controller.get_stored_info().await;
        let _ = controller.store.write(&stored_info).await.expect("write should succeed");

        // The camera reports the same value as storage, so storage is unchanged, the stored
        // MUTED state is now applied to all cameras, and the ERROR state is cleared and published.
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert_eq!(controller.get_stored_info().await, stored_info);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );

        let (_, published) = value_rx.try_recv().expect("cleared error should be published");
        assert_eq!(published, expected_published_value(&controller));
    }

    #[fuchsia::test]
    async fn test_camera_event_while_synced() {
        let (mut controller, _camera1_muted, _camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(false, vec![]).await;
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);

        // A camera event matching the stored state is not published.
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(None)), "unexpected result {res:?}");
        assert!(value_rx.try_recv().is_err());

        // A camera event that differs from the stored state is adopted and published.
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
        assert!(value_rx.try_recv().is_ok());
    }

    #[fuchsia::test]
    async fn test_set_input_states_clears_camera_error_with_unchanged_storage() {
        use std::sync::atomic::Ordering;

        // The push made by restore() fails on camera 1.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(true, vec![0]).await;
        let stored_info = controller.get_stored_info().await;
        let _ = controller.store.write(&stored_info).await.expect("write should succeed");
        assert!(
            InputController::get_cam_sw_state_from(&controller.input_device_state)
                .unwrap()
                .has_state(DeviceState::ERROR)
        );

        // Setting the stored state again applies it to the cameras. Storage is unchanged, but the
        // result must still be reported so that the ERROR state is cleared for watchers.
        let mute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };
        let res = controller.set_input_states(vec![mute_device], DeviceStateSource::SOFTWARE).await;
        let info = res.expect("set should succeed").expect("cleared error should be published");
        assert_eq!(controller.get_stored_info().await, stored_info);
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(
            InputController::get_cam_sw_state_from(&info.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
    }
    #[fuchsia::test]
    async fn test_set_mic_clears_camera_error() {
        use std::sync::atomic::Ordering;

        // The push made by restore() fails on camera 1.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(true, vec![0]).await;
        assert!(
            InputController::get_cam_sw_state_from(&controller.input_device_state)
                .unwrap()
                .has_state(DeviceState::ERROR)
        );

        // Updating only the microphone retries pushing the dirty camera software state and clears
        // the camera ERROR state even though the request does not include a camera device.
        let mute_mic = InputDevice {
            name: DEFAULT_MIC_NAME.to_string(),
            device_type: InputDeviceType::MICROPHONE,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };
        let res = controller.set_input_states(vec![mute_mic], DeviceStateSource::SOFTWARE).await;
        let info = res.expect("set should succeed").expect("updated mic should be published");
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(
            InputController::get_cam_sw_state_from(&info.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
    }

    #[fuchsia::test]
    async fn test_set_input_states_retry_after_adopting_camera_state() {
        use std::sync::atomic::Ordering;

        // Both cameras start muted. Camera 1 fails the first unmute attempt and the re-push made
        // while handling the following camera event.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(true, vec![1, 2]).await;

        let unmute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::AVAILABLE)].into(),
            state: DeviceState::AVAILABLE,
        };
        let res = controller
            .set_input_states(vec![unmute_device.clone()], DeviceStateSource::SOFTWARE)
            .await;
        assert!(res.is_err());
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(!camera2_muted.load(Ordering::Relaxed));

        // The watcher reports `false` because camera 2 unmuted. The re-push of the stored MUTED
        // state fails on camera 1, so the reported AVAILABLE state is shown while storage keeps
        // MUTED.
        let watcher_res = controller.handle_camera_event(false).await;
        assert!(matches!(watcher_res, Ok(Some(()))), "unexpected result {watcher_res:?}");
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::AVAILABLE
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );

        // Retrying set_input_states(AVAILABLE) still pushes to camera 1 even though the reported
        // state is already AVAILABLE.
        let retry_res =
            controller.set_input_states(vec![unmute_device], DeviceStateSource::SOFTWARE).await;
        assert!(retry_res.is_ok());
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert!(!camera2_muted.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn test_set_input_states_camera_resync_does_not_block_other_devices() {
        use std::sync::atomic::Ordering;

        // Stored state is MUTED. Every push (including restore) fails on camera 1.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(true, (0..100).collect()).await;
        camera1_muted.store(false, Ordering::Relaxed);
        camera2_muted.store(false, Ordering::Relaxed);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED | DeviceState::ERROR
        );

        // Muting the microphone while in-memory camera state carries ERROR re-applies the stored
        // MUTED camera state where possible, succeeds even though camera 1 fails, and keeps the
        // reported camera ERROR state in the published InputInfo.
        let mute_mic = InputDevice {
            name: DEFAULT_MIC_NAME.to_string(),
            device_type: InputDeviceType::MICROPHONE,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };
        let info = controller
            .set_input_states(vec![mute_mic], DeviceStateSource::SOFTWARE)
            .await
            .expect("mic mute should succeed")
            .expect("mic mute should be published");
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);
        assert_eq!(
            InputController::get_cam_sw_state_from(&info.input_device_state).unwrap(),
            DeviceState::MUTED | DeviceState::ERROR
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );

        // A camera event fails to re-push MUTED to camera 1 and moves to Adopted, where the
        // reported camera state is AVAILABLE while storage keeps MUTED.
        let watcher_res = controller.handle_camera_event(false).await;
        assert!(matches!(watcher_res, Ok(Some(()))), "unexpected result {watcher_res:?}");
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::AVAILABLE
        );

        // Changing the microphone while Adopted still succeeds even though the reported and
        // stored camera states differ and camera 1 keeps failing, and keeps reporting the camera
        // as AVAILABLE.
        let unmute_mic = InputDevice {
            name: DEFAULT_MIC_NAME.to_string(),
            device_type: InputDeviceType::MICROPHONE,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::AVAILABLE)].into(),
            state: DeviceState::AVAILABLE,
        };
        let info = controller
            .set_input_states(vec![unmute_mic], DeviceStateSource::SOFTWARE)
            .await
            .expect("mic unmute should succeed")
            .expect("mic unmute should be published");
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);
        assert_eq!(
            InputController::get_cam_sw_state_from(&info.input_device_state).unwrap(),
            DeviceState::AVAILABLE
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
    }

    #[fuchsia::test]
    async fn test_set_input_states_other_device_does_not_unmute_camera() {
        use std::sync::atomic::Ordering;

        // Both cameras start unmuted. The first mute fails on camera 1, so the stored state stays
        // AVAILABLE without being fully applied.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(false, vec![1]).await;
        let mute_camera = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };
        let res = controller.set_input_states(vec![mute_camera], DeviceStateSource::SOFTWARE).await;
        assert!(res.is_err());
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);

        // Another client mutes camera 1. Muting the microphone does not push the stored
        // AVAILABLE state, so the cameras stay muted.
        camera1_muted.store(true, Ordering::Relaxed);
        let mute_mic = InputDevice {
            name: DEFAULT_MIC_NAME.to_string(),
            device_type: InputDeviceType::MICROPHONE,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::MUTED)].into(),
            state: DeviceState::MUTED,
        };
        let res = controller.set_input_states(vec![mute_mic], DeviceStateSource::SOFTWARE).await;
        assert!(res.is_ok(), "unexpected result {res:?}");
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
    }

    #[fuchsia::test]
    async fn test_camera_added_reapplies_stored_mute() {
        use std::sync::atomic::Ordering;

        // The stored MUTED state is applied to both cameras at startup.
        let (mut controller, camera1_muted, camera2_muted, mut value_rx) =
            setup_multi_camera_test_with_failures(true, vec![]).await;
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);
        assert!(camera1_muted.load(Ordering::Relaxed));

        // Camera 1 restarts, which resets its software mute state, and is reported as added.
        // The watcher then reports the cameras as unmuted.
        camera1_muted.store(false, Ordering::Relaxed);
        assert_eq!(controller.handle_camera_added(), None);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");

        // The stored MUTED state is applied to the cameras again instead of being replaced.
        assert!(camera1_muted.load(Ordering::Relaxed));
        assert!(camera2_muted.load(Ordering::Relaxed));
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
        assert_eq!(
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap(),
            DeviceState::MUTED
        );
        assert!(value_rx.try_recv().is_ok());
    }

    #[fuchsia::test]
    async fn test_camera_added_while_unmuted_does_not_revert_later_external_unmute() {
        use std::sync::atomic::Ordering;

        // Both cameras start unmuted with stored AVAILABLE.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(false, vec![]).await;
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);

        // A camera is reported as added, and the watcher reports the cameras as unmuted.
        assert_eq!(controller.handle_camera_added(), None);
        assert_eq!(controller.cam_sw_sync, CamSwSync::Pending);
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(None)), "unexpected result {res:?}");

        // Another client later mutes all cameras, then unmutes camera 1. The unmute is followed
        // rather than reverted.
        camera1_muted.store(true, Ordering::Relaxed);
        camera2_muted.store(true, Ordering::Relaxed);
        let res = controller.handle_camera_event(true).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert_eq!(controller.cam_sw_sync, CamSwSync::Synced);

        camera1_muted.store(false, Ordering::Relaxed);
        let res = controller.handle_camera_event(false).await;
        assert!(matches!(res, Ok(Some(()))), "unexpected result {res:?}");
        assert!(!camera1_muted.load(Ordering::Relaxed));
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::AVAILABLE
        );
    }

    #[fuchsia::test]
    async fn test_camera_events_converge_when_one_camera_keeps_failing() {
        use std::sync::atomic::Ordering;

        // Both cameras start muted. Every push after restore() fails on camera 1.
        let (mut controller, camera1_muted, camera2_muted, _value_rx) =
            setup_multi_camera_test_with_failures(true, (1..100).collect()).await;

        let unmute_device = InputDevice {
            name: DEFAULT_CAMERA_NAME.to_string(),
            device_type: InputDeviceType::CAMERA,
            source_states: [(DeviceStateSource::SOFTWARE, DeviceState::AVAILABLE)].into(),
            state: DeviceState::AVAILABLE,
        };
        let res =
            controller.set_input_states(vec![unmute_device], DeviceStateSource::SOFTWARE).await;
        assert!(res.is_err());

        // Act as CameraWatcherAgent: report the aggregate mute state (muted only if all cameras
        // are muted) whenever it differs from the last reported value.
        let mut last_reported = true;
        let mut events = 0;
        loop {
            let aggregate =
                camera1_muted.load(Ordering::Relaxed) && camera2_muted.load(Ordering::Relaxed);
            if aggregate == last_reported {
                break;
            }
            assert!(events < 10, "camera events did not converge");
            last_reported = aggregate;
            events += 1;
            let res = controller.handle_camera_event(aggregate).await;
            assert!(res.is_ok(), "unexpected result {res:?}");
        }

        // The reported state matches the state reported by the cameras, storage keeps the user's
        // MUTED preference, and the next set_input_states call still retries the push.
        assert_eq!(controller.cam_sw_sync, CamSwSync::Adopted);
        let reported_state =
            InputController::get_cam_sw_state_from(&controller.input_device_state).unwrap();
        assert_eq!(reported_state.has_state(DeviceState::MUTED), last_reported);
        assert_eq!(
            InputController::get_cam_sw_state_from(
                &controller.get_stored_info().await.input_device_state
            )
            .unwrap(),
            DeviceState::MUTED
        );
    }
}
