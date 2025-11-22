// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::AudioInfoLoader;
use super::audio_fidl_handler::{Publisher, Publisher2};
use super::types::AudioError;
use crate::audio::types::{
    AUDIO_STREAM_TYPE_COUNT, AudioInfo, AudioStream, AudioStreamType, SetAudioStream,
};
use crate::audio::{ModifiedCounters, StreamVolumeControl, create_default_modified_counters};
use crate::{trace, trace_guard};
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use futures::StreamExt;
use futures::channel::oneshot::Sender;
use settings_common::inspect::event::{ExternalEventPublisher, SettingValuePublisher};
use settings_common::service_context::ServiceContext;
use settings_storage::device_storage::{DeviceStorage, DeviceStorageCompatible};
use settings_storage::storage_factory::{DefaultLoader, StorageAccess, StorageFactory};
use std::collections::HashMap;
use std::rc::Rc;
use {fuchsia_async as fasync, fuchsia_trace as ftrace};

pub enum Request {
    Get(ftrace::Id, Sender<AudioInfo>),
    Listen(UnboundedSender<AudioInfo>),
    Set(Vec<SetAudioStream>, ftrace::Id, Sender<Result<(), AudioError>>),
}

struct Restart;

impl StorageAccess for AudioController {
    type Storage = DeviceStorage;
    type Data = AudioInfo;
    const STORAGE_KEY: &'static str = AudioInfo::KEY;
}

pub(crate) struct AudioController {
    service_context: Rc<ServiceContext>,
    store: Rc<DeviceStorage>,
    audio_service_connected: bool,
    stream_volume_controls: HashMap<AudioStreamType, StreamVolumeControl>,
    modified_counters: ModifiedCounters,
    audio_info_loader: AudioInfoLoader,
    publisher: Option<Publisher>,
    publisher2: Option<Publisher2>,
    listeners: Vec<UnboundedSender<AudioInfo>>,
    setting_value_publisher: SettingValuePublisher<AudioInfo>,
    external_publisher: ExternalEventPublisher,
    restart_tx: UnboundedSender<Restart>,
    restart_rx: Option<UnboundedReceiver<Restart>>,
}

impl AudioController {
    pub(crate) async fn new<F>(
        service_context: Rc<ServiceContext>,
        audio_info_loader: AudioInfoLoader,
        storage_factory: Rc<F>,
        setting_value_publisher: SettingValuePublisher<AudioInfo>,
        external_publisher: ExternalEventPublisher,
    ) -> AudioController
    where
        F: StorageFactory<Storage = DeviceStorage>,
    {
        let store = storage_factory.get_store().await;
        let (restart_tx, restart_rx) = mpsc::unbounded();
        Self {
            service_context,
            store,
            stream_volume_controls: HashMap::new(),
            audio_service_connected: false,
            modified_counters: create_default_modified_counters(),
            audio_info_loader,
            publisher: None,
            publisher2: None,
            listeners: vec![],
            setting_value_publisher,
            external_publisher,
            restart_tx,
            restart_rx: Some(restart_rx),
        }
    }

    /// Restores the necessary dependencies' state on boot. Extracts the audio state from
    /// persistent storage and restores it on the local state.
    pub(crate) async fn restore(&mut self) -> AudioInfo {
        let id = ftrace::Id::new();
        trace!(id, c"restore");
        self.restore_volume_state(id, true).await
    }

    /// Restores the necessary dependencies' state on boot. Extracts the audio state from
    /// persistent storage and restores it on the local state.
    pub(crate) async fn restore_volume_state(
        &mut self,
        id: ftrace::Id,
        push_to_audio_core: bool,
    ) -> AudioInfo {
        let audio_info = self.store.get::<AudioInfo>().await;

        trace!(id, c"update volume streams from info");
        let new_streams = audio_info.streams.iter();
        let _guard = trace_guard!(id, c"check and bind");
        if let Err(e) = self.update_streams(push_to_audio_core, new_streams, id).await {
            log::error!("Failed to update streams: {e:?}");
        }
        audio_info
    }

    pub(crate) async fn get_info(&self) -> AudioInfo {
        let mut info = self.store.get::<AudioInfo>().await;
        info.modified_counters = Some(self.modified_counters.clone());
        info
    }

    pub(crate) fn register_publishers(&mut self, publisher: Publisher, publisher2: Publisher2) {
        self.publisher = Some(publisher);
        self.publisher2 = Some(publisher2);
    }

    fn register_listener(&mut self, tx: UnboundedSender<AudioInfo>) {
        self.listeners.push(tx);
    }

    fn publish(&self, new_info: AudioInfo) {
        let _ = self.setting_value_publisher.publish(&new_info);
        // Listeners always get updated.
        for listener in &self.listeners {
            let _ = listener.unbounded_send(new_info.clone());
        }
        // Watch subscribers only receive updates to streams.
        if let Some(publisher) = self.publisher.as_ref() {
            publisher.update(|info| {
                // Unwrap ok because info is always initialized.
                let info = info.as_mut().unwrap();
                let mut old_streams = info.streams.iter();
                let new_streams = new_info.streams.iter();
                for new_stream in new_streams {
                    let old_stream = old_streams
                        .find(|stream| stream.stream_type == new_stream.stream_type)
                        .expect("stream type should be found in existing streams");
                    // Watch() notifies upon changes to "legacy" stream types.
                    if (old_stream != new_stream) && new_stream.stream_type.is_legacy() {
                        *info = new_info.clone();
                        return true;
                    }
                }
                false
            });
        }
        if let Some(publisher2) = self.publisher2.as_ref() {
            publisher2.update(|info| {
                // Unwrap ok because info is always initialized.
                let info = info.as_mut().unwrap();
                let mut old_streams = info.streams.iter();
                let new_streams = new_info.streams.iter();
                for new_stream in new_streams {
                    let old_stream = old_streams
                        .find(|stream| stream.stream_type == new_stream.stream_type)
                        .expect("stream type should be found in existing streams");
                    // Watch2() notifies upon changes to any stream type.
                    if old_stream != new_stream {
                        *info = new_info.clone();
                        return true;
                    }
                }
                false
            });
        }
    }

    async fn set_volume(
        &mut self,
        volume: Vec<SetAudioStream>,
        id: ftrace::Id,
    ) -> Result<AudioInfo, AudioError> {
        let guard = trace_guard!(id, c"set volume updating counters");
        // Update counters for changed streams.
        for stream in &volume {
            // We don't care what the value of the counter is, just that it is different from the
            // previous value. We use wrapping_add to avoid eventual overflow of the counter.
            let counter = self.modified_counters.entry(stream.stream_type).or_insert(0);
            *counter = counter.wrapping_add(1);
        }
        drop(guard);

        self.update_volume_streams_from_new_streams(volume, true, id).await
    }

    async fn get_streams_array_from_map(
        &self,
        stream_map: &HashMap<AudioStreamType, StreamVolumeControl>,
    ) -> [AudioStream; AUDIO_STREAM_TYPE_COUNT] {
        let mut streams: [AudioStream; AUDIO_STREAM_TYPE_COUNT] =
            self.audio_info_loader.default_value().streams;
        for stream in &mut streams {
            if let Some(volume_control) = stream_map.get(&stream.stream_type) {
                *stream = volume_control.stored_stream;
            }
        }

        streams
    }

    async fn update_streams(
        &mut self,
        push_to_audio_core: bool,
        new_streams: impl Iterator<Item = &AudioStream>,
        id: ftrace::Id,
    ) -> Result<(), AudioError> {
        if push_to_audio_core {
            let guard = trace_guard!(id, c"push to core");
            self.check_and_bind_volume_controls(
                id,
                self.audio_info_loader.default_value().streams.iter(),
            )
            .await?;
            drop(guard);

            trace!(id, c"setting core");
            for stream in new_streams {
                if let Some(volume_control) =
                    self.stream_volume_controls.get_mut(&stream.stream_type)
                {
                    let _ = volume_control.set_volume(id, *stream).await?;
                }
            }
        } else {
            trace!(id, c"without push to core");
            self.check_and_bind_volume_controls(id, new_streams).await?;
        }

        Ok(())
    }

    async fn update_volume_streams_from_new_streams(
        &mut self,
        streams: Vec<SetAudioStream>,
        push_to_audio_core: bool,
        id: ftrace::Id,
    ) -> Result<AudioInfo, AudioError> {
        let mut new_vec = vec![];
        trace!(id, c"update volume streams from new streams");
        let calculating_guard = trace_guard!(id, c"check and bind");
        trace!(id, c"reading setting");
        let mut stored_value = self.store.get::<AudioInfo>().await;
        for set_stream in streams.iter() {
            let stored_stream = stored_value
                .streams
                .iter()
                .find(|stream| stream.stream_type == set_stream.stream_type)
                .ok_or_else(|| AudioError::InvalidArgument("stream", format!("{set_stream:?}")))?;
            new_vec.push(AudioStream {
                stream_type: stored_stream.stream_type,
                source: set_stream.source,
                user_volume_level: set_stream
                    .user_volume_level
                    .unwrap_or(stored_stream.user_volume_level),
                user_volume_muted: set_stream
                    .user_volume_muted
                    .unwrap_or(stored_stream.user_volume_muted),
            });
        }
        let new_streams = new_vec.iter();

        self.update_streams(push_to_audio_core, new_streams, id).await?;
        drop(calculating_guard);

        let guard = trace_guard!(id, c"updating streams and counters");
        stored_value.streams = self.get_streams_array_from_map(&self.stream_volume_controls).await;
        stored_value.modified_counters = Some(self.modified_counters.clone());
        drop(guard);

        let guard = trace_guard!(id, c"writing setting");
        let write_result = self.store.write(&stored_value).await;
        drop(guard);
        // Always return the stored value
        write_result.map(|_| stored_value).map_err(AudioError::WriteFailure)
    }

    /// Populates the local state with the given `streams` and binds it to the audio core service.
    async fn check_and_bind_volume_controls(
        &mut self,
        id: ftrace::Id,
        streams: impl Iterator<Item = &AudioStream>,
    ) -> Result<(), AudioError> {
        trace!(id, c"check and bind fn");
        if self.audio_service_connected {
            return Ok(());
        }

        let guard = trace_guard!(id, c"connecting to service");
        let service_result = self
            .service_context
            .connect_with_publisher::<fidl_fuchsia_media::AudioCoreMarker, _>(
                self.external_publisher.clone(),
            )
            .await;

        let audio_service = service_result.map_err(|e| {
            AudioError::ExternalFailure(
                "fuchsia.media.audio",
                "connect for audio_core".into(),
                format!("{e:?}"),
            )
        })?;

        // The stream_volume_controls are generated in two steps instead of
        // one so that if one of the bindings fails during the first loop,
        // none of the streams are modified.
        drop(guard);
        let mut stream_tuples = Vec::new();
        for stream in streams {
            trace!(id, c"create stream volume control");
            let restart_tx = self.restart_tx.clone();

            // Generate a tuple with stream type and StreamVolumeControl.
            stream_tuples.push((
                stream.stream_type,
                StreamVolumeControl::create(
                    id,
                    audio_service.clone(),
                    *stream,
                    Some(Rc::new(move || {
                        if let Err(e) = restart_tx.unbounded_send(Restart) {
                            log::error!("Failed to send restart signal: {e:?}");
                        }
                    })),
                    #[cfg(test)]
                    None,
                )
                .await?,
            ));
        }

        stream_tuples.into_iter().for_each(|(stream_type, stream_volume_control)| {
            // Ignore the previous value, if any.
            let _ = self.stream_volume_controls.insert(stream_type, stream_volume_control);
        });
        self.audio_service_connected = true;

        Ok(())
    }

    pub(crate) async fn handle(
        mut self,
        mut request_rx: UnboundedReceiver<Request>,
    ) -> fasync::Task<()> {
        let mut restart_rx: UnboundedReceiver<Restart> = self.restart_rx.take().unwrap();
        fasync::Task::local(async move {
            let mut next_request = request_rx.next();
            let mut next_restart = restart_rx.next();
            loop {
                futures::select! {
                    request = next_request => {
                        if let Some(request) = request {
                            self.handle_request(request).await;
                            next_request = request_rx.next();
                        }
                    }
                    restart = next_restart => {
                        if let Some(_) = restart {
                            self.handle_restart().await;
                            next_restart = restart_rx.next();
                        }
                    }
                }
            }
        })
    }

    async fn handle_request(&mut self, request: Request) {
        match request {
            Request::Get(id, tx) => {
                trace!(id, c"controller get");
                let res = self.get_info().await;
                let _ = tx.send(res);
            }
            Request::Listen(tx) => {
                self.register_listener(tx);
            }
            Request::Set(streams, id, tx) => {
                trace!(id, c"controller set");
                // Validate volume contains valid volume level numbers.
                for audio_stream in &streams {
                    if !audio_stream.has_valid_volume_level() {
                        let _ = tx.send(Err(AudioError::InvalidArgument(
                            "stream",
                            format!("{audio_stream:?}"),
                        )));
                        return;
                    }
                }
                let res = self.set_volume(streams, id).await.map(|mut info| {
                    info.modified_counters = Some(self.modified_counters.clone());
                    self.publish(info)
                });
                let _ = tx.send(res);
            }
        }
    }

    async fn handle_restart(&mut self) {
        let id = ftrace::Id::new();
        trace!(id, c"restart");
        self.audio_service_connected = false;
        self.stream_volume_controls.clear();
        let _ = self.restore_volume_state(id, false).await;
    }
}
