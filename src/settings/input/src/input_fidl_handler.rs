// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_controller::{InputController, InputError, Request, SetInputDevice};
use crate::types::{DeviceState, InputDeviceType, InputInfo};
use anyhow::{Error, anyhow};
use async_utils::hanging_get::server;
use fidl_fuchsia_settings::{
    Error as SettingsError, InputRequest, InputRequestStream, InputSettings, InputState,
    InputWatchResponder,
};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use settings_common::inspect::event::{
    HangingGetObserver, RequestType, ResponseType, UsagePublisher,
};

fn to_request(fidl_input_states: Vec<InputState>) -> Result<Vec<SetInputDevice>, Error> {
    fidl_input_states
        .into_iter()
        .map(|input_state| {
            // Every device requires at least a device type and state flags.
            let (Some(fidl_device_type), Some(fidl_state)) =
                (input_state.device_type, input_state.state)
            else {
                return Err(anyhow!("Failed to parse input request: missing args"));
            };
            let device_type: InputDeviceType = fidl_device_type.into();
            let device_state = DeviceState::try_from(fidl_state)?;
            let device_name = input_state.name.unwrap_or_else(|| device_type.to_string());
            Ok(SetInputDevice { name: device_name, device_type, state: device_state })
        })
        .collect()
}

pub(super) type SubscriberObject = HangingGetObserver<InputInfo, InputWatchResponder>;
type HangingGetFn = fn(&InputInfo, SubscriberObject) -> bool;
pub(super) type HangingGet = server::HangingGet<InputInfo, SubscriberObject, HangingGetFn>;
pub(super) type Publisher = server::Publisher<InputInfo, SubscriberObject, HangingGetFn>;
pub(super) type Subscriber = server::Subscriber<InputInfo, SubscriberObject, HangingGetFn>;

pub struct InputFidlHandler {
    hanging_get: HangingGet,
    controller_tx: UnboundedSender<Request>,
    usage_publisher: UsagePublisher<InputInfo>,
}

impl InputFidlHandler {
    pub(crate) fn new(
        input_controller: &mut InputController,
        usage_publisher: UsagePublisher<InputInfo>,
        initial_value: InputInfo,
    ) -> (Self, UnboundedReceiver<Request>) {
        let hanging_get = HangingGet::new(initial_value, Self::hanging_get);
        input_controller.register_publisher(hanging_get.new_publisher());
        let (controller_tx, controller_rx) = mpsc::unbounded();
        (Self { hanging_get, controller_tx, usage_publisher }, controller_rx)
    }

    fn hanging_get(info: &InputInfo, observer: SubscriberObject) -> bool {
        let (usage_responder, responder) = observer.into_parts();
        let result = usage_responder.send_and_respond(
            responder,
            |responder| responder.send(&InputSettings::from(info)),
            format!("{info:?}"),
            ResponseType::OkSome,
        );
        if let Err(e) = result {
            log::warn!("Failed to respond to watch request: {e:?}");
            return false;
        }
        true
    }

    pub fn handle_stream(&mut self, mut stream: InputRequestStream) {
        let request_handler = RequestHandler {
            subscriber: self.hanging_get.new_subscriber(),
            controller_tx: self.controller_tx.clone(),
            usage_publisher: self.usage_publisher.clone(),
        };
        fasync::Task::local(async move {
            while let Some(request) = stream.next().await {
                let request = match request {
                    Ok(request) => request,
                    Err(e) => {
                        request_handler.usage_publisher.record_stream_error(&e);
                        break;
                    }
                };
                request_handler.handle_request(request).await;
            }
        })
        .detach();
    }
}

#[derive(Debug)]
enum HandlerError {
    AlreadySubscribed,
    InvalidArgument(
        // Error used by Debug impl for inspect logs.
        #[allow(dead_code)] Error,
    ),
    ControllerStopped,
    Controller(InputError),
}

impl From<&HandlerError> for ResponseType {
    fn from(error: &HandlerError) -> Self {
        match error {
            HandlerError::AlreadySubscribed => ResponseType::AlreadySubscribed,
            HandlerError::InvalidArgument(_) => ResponseType::InvalidArgument,
            HandlerError::ControllerStopped => ResponseType::UnexpectedError,
            HandlerError::Controller(e) => ResponseType::from(e),
        }
    }
}

struct RequestHandler {
    subscriber: Subscriber,
    controller_tx: UnboundedSender<Request>,
    usage_publisher: UsagePublisher<InputInfo>,
}

impl RequestHandler {
    async fn handle_request(&self, request: InputRequest) {
        match request {
            InputRequest::Watch { responder } => {
                let usage_res = self.usage_publisher.request("Watch".to_string(), RequestType::Get);
                let observer = HangingGetObserver::new(usage_res, responder);
                if let Err(observer) = self.subscriber.register2(observer) {
                    let e = HandlerError::AlreadySubscribed;
                    let (usage_res, responder) = observer.into_parts();
                    usage_res.respond(format!("Err({e:?})"), ResponseType::from(&e));
                    drop(responder);
                }
            }
            InputRequest::Set { input_states, responder } => {
                let usage_res = self
                    .usage_publisher
                    .request(format!("Set{{input_states:{input_states:?}}}"), RequestType::Set);
                if let Err(e) = self.set(input_states).await {
                    usage_res.respond(format!("Err({e:?}"), ResponseType::from(&e));
                    let _ = responder.send(Err(SettingsError::Failed));
                } else {
                    let _ = usage_res.send_and_respond(
                        responder,
                        |responder| responder.send(Ok(())),
                        "Ok(())".to_string(),
                        ResponseType::OkNone,
                    );
                }
            }
        }
    }

    async fn set(&self, input_states: Vec<InputState>) -> Result<(), HandlerError> {
        let (set_tx, set_rx) = oneshot::channel();
        let info = to_request(input_states).map_err(HandlerError::InvalidArgument)?;
        self.controller_tx
            .unbounded_send(Request::Set(info, set_tx))
            .map_err(|_| HandlerError::ControllerStopped)?;
        set_rx
            .await
            .map_err(|_| HandlerError::ControllerStopped)
            .and_then(|res| res.map_err(HandlerError::Controller))
    }
}
