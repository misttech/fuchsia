// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::listener_logger::ListenerInspectLogger;
use crate::service_context::ExternalServiceEvent;
use futures::channel::mpsc::UnboundedSender;
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;

// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
#[derive(Clone)]
pub enum Direction {
    Request(String),
    Response(String),
}

#[derive(Clone)]
// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
pub struct UsageEvent {
    pub setting: &'static str,
    pub request_type: RequestType,
    pub direction: Direction,
    pub id: u64,
}

// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
pub trait Nameable {
    const NAME: &str;
}

#[derive(Clone)]
// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
pub struct UsagePublisher<T> {
    id_gen: Rc<Cell<u64>>,
    inspect_tx: UnboundedSender<InspectEvent>,
    listener_logger: Rc<ListenerInspectLogger>,
    _phantom: PhantomData<T>,
}

impl<T> UsagePublisher<T> {
    pub fn new(
        inspect_tx: UnboundedSender<InspectEvent>,
        listener_logger: Rc<ListenerInspectLogger>,
    ) -> Self {
        Self { id_gen: Rc::new(Cell::new(0)), inspect_tx, listener_logger, _phantom: PhantomData }
    }
}

impl<T> UsagePublisher<T>
where
    T: Nameable,
{
    pub fn request(&self, request: String, request_type: RequestType) -> UsageResponsePublisher<T> {
        let id = self.id_gen.get();
        self.id_gen.set(id.wrapping_add(1));
        let _ = self.inspect_tx.unbounded_send(InspectEvent::Usage(UsageEvent {
            setting: T::NAME,
            request_type,
            direction: Direction::Request(request),
            id,
        }));
        if let RequestType::Get = request_type {
            self.listener_logger.add_listener(T::NAME.into());
        }

        UsageResponsePublisher {
            id,
            request_type,
            inspect_tx: self.inspect_tx.clone(),
            listener_logger: Rc::clone(&self.listener_logger),
            sent: false,
            _phantom: PhantomData,
        }
    }
}

#[derive(Clone)]
pub struct UsageResponsePublisher<T> {
    id: u64,
    request_type: RequestType,
    inspect_tx: UnboundedSender<InspectEvent>,
    listener_logger: Rc<ListenerInspectLogger>,
    sent: bool,
    _phantom: PhantomData<T>,
}

impl<T> UsageResponsePublisher<T>
where
    T: Nameable,
{
    pub fn respond(mut self, response: String) {
        let _ = self.inspect_tx.unbounded_send(InspectEvent::Usage(UsageEvent {
            setting: T::NAME,
            request_type: self.request_type,
            direction: Direction::Response(response),
            id: self.id,
        }));
        if let RequestType::Get = self.request_type {
            self.listener_logger.remove_listener(T::NAME.into());
        }
        self.sent = true;
    }
}

impl<T> Drop for UsageResponsePublisher<T> {
    fn drop(&mut self) {
        if !self.sent {
            log::error!("UsageResponsePublisher dropped without sending response");
        }
    }
}

#[derive(Copy, Clone, Debug)]
// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
pub enum RequestType {
    Get,
    Set,
    OnCameraSWState(bool),
    OnButton(MediaButtons),
}

#[derive(Copy, Clone, Debug)]
// TODO(https://fxbug.dev/42166874) Remove allow once used
#[allow(dead_code)]
pub struct MediaButtons {
    pub mic_mute: Option<bool>,
    pub camera_disable: Option<bool>,
}

#[derive(Clone)]
pub enum InspectEvent {
    // TODO(https://fxbug.dev/42166874) Remove allow once used
    #[allow(dead_code)]
    SettingValue { setting: &'static str, value: String },
    // TODO(https://fxbug.dev/42166874) Remove allow once used
    #[allow(dead_code)]
    Usage(UsageEvent),
    // TODO(https://fxbug.dev/42166874) Remove allow once used
    #[allow(dead_code)]
    External(ExternalServiceEvent),
}
