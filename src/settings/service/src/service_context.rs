// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::event::{Event, Publisher};
use crate::message::base::MessengerType;
use crate::service;
use anyhow::Error;
use common::{
    EventPublisher, ExternalServiceEvent, ExternalServiceProxy as InnerExternalServiceProxy,
    ServiceContext as InnerServiceContext,
};
use fidl::endpoints::DiscoverableProtocolMarker;
use futures::future::OptionFuture;
use std::rc::Rc;

pub(crate) mod common;

#[cfg(test)]
use common::GenerateService;

/// A wrapper around service operations, allowing redirection to a nested
/// environment.
pub struct ServiceContext {
    inner: Rc<InnerServiceContext>,
    delegate: Option<service::message::Delegate>,
}

impl ServiceContext {
    #[cfg(test)]
    pub(crate) fn new(
        generate_service: Option<GenerateService>,
        delegate: Option<service::message::Delegate>,
    ) -> Self {
        let inner = Rc::new(InnerServiceContext::new(generate_service));
        Self { inner, delegate }
    }

    pub(crate) fn new_from_common(
        inner: Rc<InnerServiceContext>,
        delegate: Option<service::message::Delegate>,
    ) -> Self {
        Self { inner, delegate }
    }

    async fn make_publisher(&self) -> Option<Publisher> {
        let maybe: OptionFuture<_> = self
            .delegate
            .as_ref()
            .map(|delegate| Publisher::create(delegate, MessengerType::Unbound))
            .into();
        maybe.await
    }

    /// Connect to a service with the given ProtocolMarker.
    ///
    /// If a GenerateService was specified at creation, the name of the service marker will be used
    /// to generate a service.
    pub(crate) async fn connect<P: DiscoverableProtocolMarker>(
        &self,
    ) -> Result<ExternalServiceProxy<P::Proxy>, Error> {
        self.inner.connect::<P, Publisher, _>(|| self.make_publisher()).await
    }

    pub(crate) async fn connect_with_publisher<P: DiscoverableProtocolMarker>(
        &self,
        publisher: Publisher,
    ) -> Result<ExternalServiceProxy<P::Proxy>, Error> {
        self.inner.connect_with_publisher::<P, Publisher>(publisher).await
    }
}

impl EventPublisher for Publisher {
    fn send_event(&self, event: ExternalServiceEvent) {
        Publisher::send_event(self, Event::ExternalServiceEvent(event))
    }
}

pub(crate) type ExternalServiceProxy<P> = InnerExternalServiceProxy<P, Publisher>;
