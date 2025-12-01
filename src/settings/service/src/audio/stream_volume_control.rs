// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::audio::types::{AudioError, AudioStream};
use crate::audio::utils::round_volume_level;
#[cfg(test)]
use crate::clock;
use fidl::endpoints::create_proxy;
use fidl_fuchsia_media::Usage2;
use fidl_fuchsia_media_audio::VolumeControlProxy;
use futures::TryStreamExt;
#[cfg(test)]
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot::Sender;
use settings_common::inspect::event::ExternalEventPublisher;
#[cfg(test)]
use settings_common::service_context::ExternalServiceEvent;
use settings_common::service_context::ExternalServiceProxy;
use settings_common::{call, trace, trace_guard};
use std::rc::Rc;
use {fuchsia_async as fasync, fuchsia_trace as ftrace};

#[cfg(test)]
const PUBLISHER_EVENT_NAME: &str = "volume_control_events";
const CONTROLLER_ERROR_DEPENDENCY: &str = "fuchsia.media.audio";
#[cfg(test)]
const UNKNOWN_INSPECT_STRING: &str = "unknown";

/// Closure definition for an action that can be triggered by ActionFuse.
pub(crate) type ExitAction = Rc<dyn Fn()>;

// Stores an AudioStream and a VolumeControl proxy bound to the AudioCore
// service for |stored_stream|'s stream type. |proxy| is set to None if it
// fails to bind to the AudioCore service. |early_exit_action| specifies a
// closure to be run if the StreamVolumeControl exits prematurely.
pub struct StreamVolumeControl {
    pub stored_stream: AudioStream,
    proxy: Option<VolumeControlProxy>,
    audio_service: ExternalServiceProxy<fidl_fuchsia_media::AudioCoreProxy, ExternalEventPublisher>,
    early_exit_action: Option<ExitAction>,
    #[cfg(test)]
    publisher: Option<UnboundedSender<ExternalServiceEvent>>,
    listen_exit_tx: Option<Sender<()>>,
}

impl Drop for StreamVolumeControl {
    fn drop(&mut self) {
        if let Some(exit_tx) = self.listen_exit_tx.take() {
            // Do not signal exit if receiver is already closed.
            if exit_tx.is_canceled() {
                return;
            }

            // Consider panic! is likely to be abort in the drop method, only log info for
            // unbounded_send failure.
            exit_tx.send(()).unwrap_or_else(|_| {
                log::warn!("StreamVolumeControl::drop, exit_tx failed to send exit signal")
            });
        }
    }
}

impl StreamVolumeControl {
    pub(crate) async fn create(
        id: ftrace::Id,
        audio_service: ExternalServiceProxy<
            fidl_fuchsia_media::AudioCoreProxy,
            ExternalEventPublisher,
        >,
        stream: AudioStream,
        early_exit_action: Option<ExitAction>,
        #[cfg(test)] publisher: Option<UnboundedSender<ExternalServiceEvent>>,
    ) -> Result<Self, AudioError> {
        // Stream input should be valid. Input comes from restore should be valid
        // and from set request has the validation.
        assert!(stream.has_valid_volume_level());

        trace!(id, c"StreamVolumeControl ctor");
        let mut control = StreamVolumeControl {
            stored_stream: stream,
            proxy: None,
            audio_service: audio_service,
            listen_exit_tx: None,
            early_exit_action,
            #[cfg(test)]
            publisher,
        };

        control.bind_volume_control(id).await?;
        Ok(control)
    }

    pub(crate) async fn set_volume(
        &mut self,
        id: ftrace::Id,
        stream: AudioStream,
    ) -> Result<(), AudioError> {
        assert_eq!(self.stored_stream.stream_type, stream.stream_type);
        // Stream input should be valid. Input comes from restore should be valid
        // and from set request has the validation.
        assert!(stream.has_valid_volume_level());

        // Try to create and bind a new VolumeControl.
        if self.proxy.is_none() {
            self.bind_volume_control(id).await?;
        }

        // Round volume level from user input.
        let mut new_stream_value = stream;
        new_stream_value.user_volume_level = round_volume_level(stream.user_volume_level);

        let proxy = self.proxy.as_ref().expect("no volume control proxy");

        if (self.stored_stream.user_volume_level - new_stream_value.user_volume_level).abs()
            > f32::EPSILON
        {
            if let Err(e) = proxy.set_volume(new_stream_value.user_volume_level) {
                self.stored_stream = new_stream_value;
                return Err(AudioError::ExternalFailure(
                    CONTROLLER_ERROR_DEPENDENCY,
                    "set volume".into(),
                    format!("{e:?}"),
                ));
            }
        }

        if self.stored_stream.user_volume_muted != new_stream_value.user_volume_muted {
            if let Err(e) = proxy.set_mute(stream.user_volume_muted) {
                self.stored_stream = new_stream_value;
                return Err(AudioError::ExternalFailure(
                    CONTROLLER_ERROR_DEPENDENCY,
                    "set mute".into(),
                    format!("{e:?}"),
                ));
            }
        }

        self.stored_stream = new_stream_value;
        Ok(())
    }

    async fn bind_volume_control(&mut self, id: ftrace::Id) -> Result<(), AudioError> {
        trace!(id, c"bind volume control");
        if self.proxy.is_some() {
            return Ok(());
        }

        let (vol_control_proxy, server_end) = create_proxy();
        let stream_type = self.stored_stream.stream_type;
        let usage = Usage2::RenderUsage(stream_type.into());

        let guard = trace_guard!(id, c"bind usage volume control");
        if let Err(e) = call!(self.audio_service => bind_usage_volume_control2(&usage, server_end))
        {
            return Err(AudioError::ExternalFailure(
                CONTROLLER_ERROR_DEPENDENCY,
                format!("bind_usage_volume_control2 for audio_core {usage:?}").into(),
                format!("{e:?}"),
            ));
        }
        drop(guard);

        let guard = trace_guard!(id, c"set values");
        // Once the volume control is bound, apply the persisted audio settings to it.
        if let Err(e) = vol_control_proxy.set_volume(self.stored_stream.user_volume_level) {
            return Err(AudioError::ExternalFailure(
                CONTROLLER_ERROR_DEPENDENCY,
                format!("set_volume for vol_control {stream_type:?}").into(),
                format!("{e:?}"),
            ));
        }

        if let Err(e) = vol_control_proxy.set_mute(self.stored_stream.user_volume_muted) {
            return Err(AudioError::ExternalFailure(
                CONTROLLER_ERROR_DEPENDENCY,
                "set_mute for vol_control".into(),
                format!("{e:?}"),
            ));
        }
        drop(guard);

        if let Some(exit_tx) = self.listen_exit_tx.take() {
            // exit_rx needs this signal to end leftover spawn.
            exit_tx.send(()).expect(
                "StreamVolumeControl::bind_volume_control, listen_exit_tx failed to send exit \
                signal",
            );
        }

        trace!(id, c"setup listener");

        let (exit_tx, mut exit_rx) = futures::channel::oneshot::channel::<()>();
        let mut volume_events = vol_control_proxy.take_event_stream();
        let early_exit_action = self.early_exit_action.clone();
        fasync::Task::local({
            #[cfg(test)]
            let publisher = self.publisher.clone();
            async move {
                let id = ftrace::Id::new();
                trace!(id, c"bind volume handler");
                loop {
                    futures::select! {
                        _ = exit_rx => {
                            trace!(id, c"exit");
                            #[cfg(test)]
                            {
                                // Send UNKNOWN_INSPECT_STRING for request-related args because it
                                // can't be tied back to the event that caused the proxy to close.
                                if let Some(publisher) = publisher {
                                    let _ = publisher.unbounded_send(
                                        ExternalServiceEvent::Closed(
                                            PUBLISHER_EVENT_NAME,
                                            UNKNOWN_INSPECT_STRING.into(),
                                            UNKNOWN_INSPECT_STRING.into(),
                                            clock::inspect_format_now().into(),
                                        )
                                    );
                                }
                            }
                            return;
                        }
                        volume_event = volume_events.try_next() => {
                            trace!(id, c"volume_event");
                            if let Err(_) | Ok(None) = volume_event {
                                if let Some(action) = early_exit_action {
                                    (action)();
                                }
                                return;
                            }
                        }
                    }
                }
            }
        })
        .detach();

        self.listen_exit_tx = Some(exit_tx);
        self.proxy = Some(vol_control_proxy);
        Ok(())
    }
}
