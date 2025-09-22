// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::{
    AgentError, Context as AgentContext, Invocation, InvocationResult, Lifespan, Payload,
};
use crate::base::SettingType;
use crate::event::{Event, Publisher, camera_watcher};
use crate::handler::base::{Payload as HandlerPayload, Request};
use crate::message::base::Audience;
use crate::service_context::ServiceContext;
use crate::{service, trace, trace_guard};
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedSender;
use settings_camera::connect_to_camera;
use settings_common::inspect::event::ExternalEventPublisher;
use std::collections::HashSet;
use std::rc::Rc;

use super::{AgentCreator, CreationFunc};

/// Setting types that the camera watcher agent will send updates to, if they're
/// available on the device.
fn get_event_setting_types() -> HashSet<SettingType> {
    vec![SettingType::Input].into_iter().collect()
}

pub(crate) fn create_registrar(
    muted_txs: Vec<UnboundedSender<bool>>,
    external_publisher: ExternalEventPublisher,
) -> AgentCreator {
    AgentCreator {
        debug_id: "CameraWatcherAgent",
        create: CreationFunc::Dynamic(Rc::new(move |context| {
            let muted_txs = muted_txs.clone();
            let external_publisher = external_publisher.clone();
            Box::pin(async move {
                CameraWatcherAgent::create(context, muted_txs, external_publisher).await;
            })
        })),
    }
}

// TODO(https://fxbug.dev/42149412): Extract common template from agents.
pub(crate) struct CameraWatcherAgent {
    publisher: Publisher,
    messenger: service::message::Messenger,

    /// Settings to send camera watcher events to.
    recipient_settings: HashSet<SettingType>,
    /// Sends an event whenever camera muted state changes. The `bool`
    /// represents whether the camera is muted or not.
    muted_txs: Vec<UnboundedSender<bool>>,
    external_publisher: ExternalEventPublisher,
}

impl CameraWatcherAgent {
    pub(crate) async fn create(
        context: AgentContext,
        muted_txs: Vec<UnboundedSender<bool>>,
        external_publisher: ExternalEventPublisher,
    ) {
        let mut agent = CameraWatcherAgent {
            publisher: context.get_publisher(),
            messenger: context
                .create_messenger()
                .await
                .expect("messenger should be created for CameraWatchAgent"),
            recipient_settings: context
                .available_components
                .intersection(&get_event_setting_types())
                .cloned()
                .collect::<HashSet<SettingType>>(),
            muted_txs,
            external_publisher,
        };

        let mut receptor = context.receptor;
        fasync::Task::local(async move {
            let id = fuchsia_trace::Id::new();
            let guard = trace_guard!(id, c"camera watcher agent");
            while let Ok((payload, client)) = receptor.next_of::<Payload>().await {
                trace!(id, c"payload");
                if let Payload::Invocation(invocation) = payload {
                    let _ = client.reply(Payload::Complete(agent.handle(invocation).await).into());
                }
            }
            drop(guard);

            log::info!("Camera watcher agent done processing requests");
        })
        .detach()
    }

    async fn handle(&mut self, invocation: Invocation) -> InvocationResult {
        match invocation.lifespan {
            Lifespan::Initialization => Err(AgentError::UnhandledLifespan),
            Lifespan::Service => self.handle_service_lifespan(invocation.service_context).await,
        }
    }

    async fn handle_service_lifespan(
        &mut self,
        service_context: Rc<ServiceContext>,
    ) -> InvocationResult {
        match connect_to_camera(service_context.common_context(), self.external_publisher.clone())
            .await
        {
            Ok(camera_device_client) => {
                let mut event_handler = EventHandler {
                    muted_txs: self.muted_txs.clone(),
                    publisher: self.publisher.clone(),
                    messenger: self.messenger.clone(),
                    recipient_settings: self.recipient_settings.clone(),
                    sw_muted: false,
                };
                fasync::Task::local(async move {
                    let id = fuchsia_trace::Id::new();
                    // Here we don't care about hw_muted state because the input service would pick
                    // up mute changes directly from the switch. We care about sw changes because
                    // other clients of the camera3 service could change the sw mute state but not
                    // notify the settings service.
                    trace!(id, c"camera_watcher_agent_handler");
                    while let Ok((sw_muted, _hw_muted)) =
                        camera_device_client.watch_mute_state().await
                    {
                        trace!(id, c"event");
                        event_handler.handle_event(sw_muted);
                    }
                })
                .detach();

                Ok(())
            }
            Err(e) => {
                log::error!("Unable to watch camera device: {:?}", e);
                Err(AgentError::UnexpectedError)
            }
        }
    }
}

struct EventHandler {
    muted_txs: Vec<UnboundedSender<bool>>,
    publisher: Publisher,
    messenger: service::message::Messenger,
    recipient_settings: HashSet<SettingType>,
    sw_muted: bool,
}

impl EventHandler {
    fn handle_event(&mut self, sw_muted: bool) {
        if self.sw_muted != sw_muted {
            self.sw_muted = sw_muted;
            self.send_event(sw_muted);
        }
    }

    fn send_event(&self, muted: bool) {
        for muted_tx in &self.muted_txs {
            let _ = muted_tx.unbounded_send(muted);
        }

        self.publisher.send_event(Event::CameraUpdate(camera_watcher::Event::OnSWMuteState(muted)));
        let setting_request: Request = Request::OnCameraSWState(muted);

        // Send the event to all the interested setting types that are also available.
        for setting_type in self.recipient_settings.iter() {
            // Ignore the receptor result.
            let _ = self.messenger.message(
                HandlerPayload::Request(setting_request.clone()).into(),
                Audience::Address(service::Address::Handler(*setting_type)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event;
    use crate::message::base::{MessageEvent, MessengerType};
    use crate::message::receptor::Receptor;
    use crate::tests::helpers::{
        create_messenger_and_publisher, create_messenger_and_publisher_from_hub,
        create_receptor_for_setting_type,
    };
    use assert_matches::assert_matches;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use settings_test_common::fakes::service::ServiceRegistry;

    // Tests that the initialization lifespan is not handled.
    #[fuchsia::test(allow_stalls = false)]
    async fn initialization_lifespan_is_unhandled() {
        // Setup messengers needed to construct the agent.
        let (messenger, publisher) = create_messenger_and_publisher().await;
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        // Construct the agent.
        let mut agent = CameraWatcherAgent {
            muted_txs: vec![],
            publisher,
            messenger,
            recipient_settings: HashSet::new(),
            external_publisher,
        };

        // Try to initiatate the initialization lifespan.
        let result = agent
            .handle(Invocation {
                lifespan: Lifespan::Initialization,
                service_context: Rc::new(ServiceContext::new(None, None)),
            })
            .await;

        assert!(matches!(result, Err(AgentError::UnhandledLifespan)));
    }

    // Tests that the agent cannot start without a camera3 service.
    #[fuchsia::test(allow_stalls = false)]
    async fn when_camera3_inaccessible_returns_err() {
        // Setup messengers needed to construct the agent.
        let (messenger, publisher) = create_messenger_and_publisher().await;
        let (event_tx, _event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(event_tx);

        // Construct the agent.
        let mut agent = CameraWatcherAgent {
            muted_txs: vec![],
            publisher,
            messenger,
            recipient_settings: HashSet::new(),
            external_publisher,
        };

        let service_context = Rc::new(ServiceContext::new(
            // Create a service registry without a camera3 service interface.
            Some(ServiceRegistry::serve(ServiceRegistry::create())),
            None,
        ));

        // Try to initiate the Service lifespan without providing the camera3 fidl interface.
        let result =
            agent.handle(Invocation { lifespan: Lifespan::Service, service_context }).await;
        assert!(matches!(result, Err(AgentError::UnexpectedError)));
    }

    // Tests that events can be sent to the intended recipients.
    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_proxies_event() {
        let service_message_hub = service::MessageHub::create_hub();
        let (messenger, publisher) =
            create_messenger_and_publisher_from_hub(&service_message_hub).await;

        // Get the messenger's signature and the receptor for agents. We need
        // a different messenger below because a broadcast would not send a message
        // to itself. The signature is used to delete the original messenger for this
        // receptor.
        let event_receptor = service::build_event_listener(&service_message_hub).await;

        // Get the messenger's signature and the receptor for agents. We need
        // a different messenger below because a broadcast would not send a message
        // to itself. The signature is used to delete the original messenger for this
        // receptor.
        let handler_receptor: Receptor =
            create_receptor_for_setting_type(&service_message_hub, SettingType::Unknown).await;

        let (tx1, mut rx1) = mpsc::unbounded();
        let (tx2, mut rx2) = mpsc::unbounded();

        let mut event_handler = EventHandler {
            muted_txs: vec![tx1.clone(), tx2.clone()],
            publisher,
            messenger,
            recipient_settings: vec![SettingType::Unknown].into_iter().collect(),
            sw_muted: false,
        };

        // Send the events.
        event_handler.handle_event(true);

        // Delete the messengers for the receptors we're selecting below. This
        // will allow the `select!` to eventually hit the `complete` case.
        service_message_hub.delete(handler_receptor.get_signature());
        service_message_hub.delete(event_receptor.get_signature());

        let mut agent_received_sw_mute = false;
        let mut handler_received_event = false;
        let mut channel_received = 0;

        let fused_event = event_receptor.fuse();
        let fused_setting_handler = handler_receptor.fuse();
        let mut next_rx1 = rx1.next();
        let mut next_rx2 = rx2.next();
        futures::pin_mut!(fused_event, fused_setting_handler);

        // Loop over the select so we can handle the messages as they come in. When all messages
        // have been handled, due to the messengers being deleted above, the complete branch should
        // be hit to break out of the loop.
        loop {
            futures::select! {
                message = fused_event.select_next_some() => {
                    if let MessageEvent::Message(service::Payload::Event(event::Payload::Event(
                        event::Event::CameraUpdate(event)
                    )), _) = message
                    {
                        match event {
                            event::camera_watcher::Event::OnSWMuteState(muted) => {
                                assert!(muted);
                                agent_received_sw_mute = true;
                            }
                        }
                    }
                },
                message = fused_setting_handler.select_next_some() => {
                    if let MessageEvent::Message(
                        service::Payload::Setting(HandlerPayload::Request(
                            Request::OnCameraSWState(_muted))),
                        _,
                    ) = message
                    {
                        handler_received_event = true;
                    }
                }
                event = next_rx1 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx1.close_channel();
                    channel_received += 1;
                }
                event = next_rx2 => {
                    let Some(muted) = event else {
                        continue;
                    };
                    assert!(muted);
                    // Close channel so we can exit select loop.
                    tx2.close_channel();
                    channel_received += 1;
                }
                complete => break,
            }
        }

        assert!(agent_received_sw_mute);
        assert!(handler_received_event);
        assert_eq!(channel_received, 2);
    }

    // Tests that events are not sent to unavailable settings.
    #[fuchsia::test(allow_stalls = false)]
    async fn event_handler_sends_no_events_if_no_settings_available() {
        let service_message_hub = service::MessageHub::create_hub();
        let (messenger, publisher) =
            create_messenger_and_publisher_from_hub(&service_message_hub).await;
        let handler_address = service::Address::Handler(SettingType::Unknown);
        let verification_request = Request::Get;

        // Get the messenger's signature and the receptor for agents. We need
        // a different messenger below because a broadcast would not send a message
        // to itself. The signature is used to delete the original messenger for this
        // receptor.
        let mut handler_receptor: Receptor = service_message_hub
            .create(MessengerType::Addressable(handler_address))
            .await
            .expect("Unable to create handler receptor")
            .1;

        // Declare all settings as unavailable so that no events are sent.
        let mut event_handler = EventHandler {
            muted_txs: vec![],
            publisher,
            messenger,
            recipient_settings: HashSet::new(),
            sw_muted: false,
        };

        // Send the events
        event_handler.handle_event(true);

        // Send an arbitrary request that should be the next payload received.
        let _ = service_message_hub
            .create(MessengerType::Unbound)
            .await
            .expect("Unable to create messenger")
            .0
            .message(
                HandlerPayload::Request(verification_request.clone()).into(),
                Audience::Address(handler_address),
            );

        // Delete the messengers for the receptors we're selecting below. This will allow the while
        // loop below to eventually finish.
        service_message_hub.delete(handler_receptor.get_signature());

        assert_matches!(
            handler_receptor.next_of::<HandlerPayload>().await,
            Ok((HandlerPayload::Request(request), _))
                if request == verification_request
        )
    }
}
