// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::{AgentCreator, AgentError, Context, CreationFunc, Payload};
use crate::storage::testing::InMemoryStorageFactory;
use crate::tests::scaffold;
use crate::{event, service, EnvironmentBuilder};
use assert_matches::assert_matches;
use fuchsia_async as fasync;
use futures::future::LocalBoxFuture;
use futures::lock::Mutex;
use std::rc::Rc;

const ENV_NAME: &str = "settings_service_event_test_environment";

// Exercises the event publishing path from agents.
#[fuchsia::test(allow_stalls = false)]
async fn test_agent_event_propagation() {
    let agent_publisher: Rc<Mutex<Option<event::Publisher>>> = Rc::new(Mutex::new(None));
    let delegate: Rc<Mutex<Option<service::message::Delegate>>> = Rc::new(Mutex::new(None));

    // Capturing the context allows retrieving the publisher meant for the
    // agent.
    let publisher_capture = agent_publisher.clone();

    // Capturing the delegate allows registering a listener to published events.
    let cloned_delegate = delegate.clone();

    // Upon instantiation, the subscriber will capture the event message
    // delegate.
    let create_subscriber = Rc::new(
        move |captured_delegate: service::message::Delegate| -> LocalBoxFuture<'static, ()> {
            let delegate = cloned_delegate.clone();
            Box::pin(async move {
                *delegate.lock().await = Some(captured_delegate);
            })
        },
    );

    // This agent simply captures the context and returns unhandled for all
    // subsequent invocations (allowing the authority to progress).

    let f = Rc::new(move |mut context: Context| -> LocalBoxFuture<'static, ()> {
        let publisher_capture = publisher_capture.clone();

        Box::pin(async move {
            *publisher_capture.lock().await = Some(context.get_publisher());

            fasync::Task::local(async move {
                let _ = &context;
                while let Ok((Payload::Invocation(_), client)) =
                    context.receptor.next_of::<Payload>().await
                {
                    let _ =
                        client.reply(Payload::Complete(Err(AgentError::UnhandledLifespan)).into());
                }
            })
            .detach();
        })
    });

    let create_agent = AgentCreator { debug_id: "TestAgent", create: CreationFunc::Dynamic(f) };

    let _ = EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
        .event_subscribers(&[scaffold::event::subscriber::Blueprint::create(create_subscriber)])
        .agents(vec![create_agent])
        .spawn_and_get_protocol_connector(ENV_NAME)
        .await
        .unwrap();

    let service_delegate =
        delegate.clone().lock().await.take().expect("Should have captured event factory");
    let mut receptor = service::build_event_listener(&service_delegate).await;

    let sent_event = event::Event::Custom("test");

    let publisher = agent_publisher.lock().await.take().expect("Should have captured publisher");
    publisher.send_event(sent_event.clone());

    assert_matches!(
        receptor.next_of::<event::Payload>().await.expect("Should have received broadcast").0,
        event::Payload::Event(broadcasted_event) if broadcasted_event == sent_event);
}
