// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use async_utils::event::Event;
use fidl::endpoints::{ClientEnd, ServerEnd};
use fidl_fuchsia_power_broker::{
    self as fpb, ElementControlRequest, ElementControlRequestStream, LeaseControlMarker,
    LeaseControlRequest, LeaseControlRequestStream, LeaseError, LeaseStatus, LessorRequest,
    LessorRequestStream, StatusRequest, StatusRequestStream, TopologyRequest,
    TopologyRequestStream,
};
use fpb::ElementSchema;
use fuchsia_async::Task;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use futures::prelude::*;
use futures::select;
use inspect_format::constants::DEFAULT_VMO_SIZE_BYTES as DEFAULT_INSPECT_VMO;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use zx::AsHandleRef;

use crate::broker::{Broker, CurrentLevelSubscriber, LeaseID};
use crate::topology::{ElementID, IndexedPowerLevel};

mod broker;
mod credentials;
mod inspect;
mod topology;

/// Wraps all hosted protocols into a single type that can be matched against
/// and dispatched.
enum IncomingRequest {
    Topology(TopologyRequestStream),
}

struct ElementHandlers {
    runner: Option<ElementRunnerHandler>,
    status: Vec<StatusChannelHandler>,
}

impl ElementHandlers {
    fn new() -> Self {
        Self { runner: None, status: Vec::new() }
    }
}

struct BrokerSvc {
    broker: Rc<RefCell<Broker>>,
    element_handlers: Rc<RefCell<HashMap<ElementID, ElementHandlers>>>,
}

impl BrokerSvc {
    fn new() -> Self {
        Self {
            broker: Rc::new(RefCell::new(Broker::new(
                component::inspector().root().create_child("broker"),
            ))),
            element_handlers: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    async fn run_lessor(
        self: Rc<Self>,
        element_id: ElementID,
        element_name: String,
        stream: LessorRequestStream,
    ) -> Result<(), Error> {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each(|request| async {
                let debug_info = format!("Lessor<{}:{}>", &element_name, &element_id);
                match request {
                    LessorRequest::Lease { level, responder } => {
                        log::debug!("{debug_info}: Leasing @ {level}");
                        let (client, server_end) =
                            fidl::endpoints::create_endpoints::<LeaseControlMarker>();
                        let lease_control_koid =
                            server_end.channel().as_handle_ref().get_koid().unwrap();
                        let resp = {
                            let mut broker = self.broker.borrow_mut();
                            let Some(level) =
                                broker.get_level_index(&element_id, &level).map(|l| l.clone())
                            else {
                                return responder
                                    .send(Err(LeaseError::InvalidLevel))
                                    .context("send failed");
                            };
                            broker.acquire_lease(&element_id, level, lease_control_koid)
                        };
                        match resp {
                            Ok(lease) => {
                                log::debug!("{debug_info}: Lease granted: {lease:?}");
                                let lease_id = lease.id.clone();
                                log::debug!(
                                    "{debug_info}: Spawning lease control task for {lease_id}");
                                Task::local({
                                    let svc = self.clone();
                                    async move {
                                        if let Err(err) = svc
                                            .run_lease_control(&lease.id, server_end.into_stream())
                                            .await
                                        {
                                            log::debug!("{debug_info}: run_lease_control err: {:?}", err);
                                        }
                                        // When the channel is closed, drop the lease.
                                        log::debug!(
                                            "{debug_info}: Channel closed, dropping lease {lease_id}"
                                        );
                                        let mut broker = svc.broker.borrow_mut();
                                        if let Err(err) = broker.drop_lease(&lease.id) {
                                            log::error!("{debug_info}: drop_lease {lease_id} failed: {:?}", err);
                                        }
                                    }
                                })
                                .detach();
                                responder.send(Ok(client)).context("send failed")
                            }
                            Err(err) => responder.send(Err(err.into())).context("send failed"),
                        }
                    }
                    LessorRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("{debug_info}: received unknown LessorRequest: {ordinal}");
                        todo!()
                    }
                }
            })
            .await
    }

    async fn run_lease_control(
        &self,
        lease_id: &LeaseID,
        stream: LeaseControlRequestStream,
    ) -> Result<(), Error> {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each(|request| async {
                match request {
                    LeaseControlRequest::WatchStatus {
                        last_status,
                        responder,
                    } => {
                        log::debug!(
                            "WatchStatus({:?}, {:?})",
                            lease_id,
                            &last_status
                        );
                        let mut receiver = {
                            let mut broker = self.broker.borrow_mut();
                            broker.watch_lease_status(lease_id)
                        };
                        while let Some(next) = receiver.next().await {
                            log::debug!(
                                "receiver.next = {:?}, last_status = {:?}",
                                &next,
                                last_status
                            );
                            let status = next.unwrap_or(LeaseStatus::Unknown);
                            if last_status != LeaseStatus::Unknown && last_status == status {
                                log::debug!(
                                    "WatchStatus: status has not changed, watching for next update...",
                                );
                                continue;
                            } else {
                                log::debug!(
                                    "WatchStatus: sending new status: {:?}", &status,
                                );
                                return responder.send(status).context("send failed");
                            }
                        }
                        Err(anyhow::anyhow!("Receiver closed, element is no longer available."))
                    }
                    LeaseControlRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("Received unknown LeaseControlRequest: {ordinal}");
                        todo!()
                    }
                }
            })
            .await
    }

    async fn run_element_control(
        self: Rc<Self>,
        element_id: ElementID,
        element_name: String,
        stream: ElementControlRequestStream,
    ) -> Result<(), Error> {
        let res = stream
            .map(|result| result.context("failed request"))
            .try_for_each(|request| {
                self.clone().handle_element_control_request(
                    &element_id,
                    element_name.clone(),
                    request,
                )
            })
            .await;
        log::debug!("ElementControl stream is closed, removing element ({element_id:?})...");
        let mut broker = self.broker.borrow_mut();
        broker.remove_element(&element_id);

        // Clean up ElementHandlers.
        self.element_handlers.borrow_mut().remove(&element_id);
        log::debug!("Element ({element_id:?}) removed.");
        res
    }

    async fn handle_element_control_request(
        self: Rc<Self>,
        element_id: &ElementID,
        element_name: String,
        request: ElementControlRequest,
    ) -> Result<(), Error> {
        let debug_info = format!("ElementControl<{}:{}>", &element_name, &element_id);
        match request {
            ElementControlRequest::OpenStatusChannel { status_channel, .. } => {
                log::debug!("{debug_info}: OpenStatusChannel");
                let svc = self.clone();
                svc.create_status_channel_handler(element_id.clone(), status_channel).await
            }
            ElementControlRequest::RegisterDependencyToken {
                token,
                dependency_type,
                responder,
            } => {
                log::debug!("{debug_info}: RegisterDependencyToken({token:?})");
                let mut broker = self.broker.borrow_mut();
                let res =
                    broker.register_dependency_token(element_id, token.into(), dependency_type);
                log::debug!(
                    "{debug_info}: RegisterDependencyToken register_credentials = ({res:?})"
                );
                responder.send(res.map_err(Into::into)).context("send failed")
            }
            ElementControlRequest::UnregisterDependencyToken { token, responder } => {
                log::debug!("{debug_info}: UnregisterDependencyToken({token:?})");
                let mut broker = self.broker.borrow_mut();
                let res = broker.unregister_dependency_token(element_id, token.into());
                log::debug!(
                    "{debug_info}: UnregisterDependencyToken unregister_credentials = ({res:?})"
                );
                responder.send(res.map_err(Into::into)).context("send failed")
            }
            ElementControlRequest::_UnknownMethod { ordinal, .. } => {
                log::warn!("{debug_info}: Received unknown ElementControlRequest: {ordinal}");
                todo!()
            }
        }
    }

    fn validate_and_unpack_add_element_payload(
        payload: ElementSchema,
    ) -> Result<
        (
            String,
            u8,
            Vec<u8>,
            Vec<fpb::LevelDependency>,
            Option<ServerEnd<fpb::LessorMarker>>,
            Option<ServerEnd<fpb::ElementControlMarker>>,
            ClientEnd<fpb::ElementRunnerMarker>,
        ),
        fpb::AddElementError,
    > {
        let Some(element_name) = payload.element_name else {
            return Err(fpb::AddElementError::Invalid);
        };
        let Some(initial_current_level) = payload.initial_current_level else {
            return Err(fpb::AddElementError::Invalid);
        };
        let Some(valid_levels) = payload.valid_levels else {
            return Err(fpb::AddElementError::Invalid);
        };
        let Some(element_runner) = payload.element_runner else {
            return Err(fpb::AddElementError::Invalid);
        };
        let level_dependencies = payload.dependencies.unwrap_or(vec![]);
        Ok((
            element_name,
            initial_current_level,
            valid_levels,
            level_dependencies,
            payload.lessor_channel,
            payload.element_control,
            element_runner,
        ))
    }

    async fn run_topology(self: Rc<Self>, stream: TopologyRequestStream) -> Result<(), Error> {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each(|request| async {
                match request {
                    TopologyRequest::AddElement { payload, responder } => {
                        log::debug!("AddElement({:?})", &payload);
                        let Ok((
                            element_name,
                            initial_current_level,
                            valid_levels,
                            level_dependencies,
                            lessor_channel,
                            element_control,
                            element_runner,
                        )) = Self::validate_and_unpack_add_element_payload(payload)
                        else {
                            return responder
                                .send(Err(fpb::AddElementError::Invalid))
                                .context("send failed");
                        };
                        let res = {
                            let mut broker = self.broker.borrow_mut();
                            broker.add_element(
                                &element_name,
                                initial_current_level,
                                valid_levels,
                                level_dependencies,
                            )
                        };
                        log::debug!("AddElement add_element = {:?}", res);
                        match res {
                            Ok(element_id) => {
                                self.element_handlers
                                    .borrow_mut()
                                    .insert(element_id.clone(), ElementHandlers::new());
                                let mut runner = ElementRunnerHandler::new(
                                    element_id.clone(),
                                    element_name.clone(),
                                );
                                runner.start(self.broker.clone(), element_runner.into_proxy());
                                self.element_handlers
                                    .borrow_mut()
                                    .entry(element_id.clone())
                                    .and_modify(|e| {
                                        e.runner = Some(runner);
                                    });
                                if let Some(element_control) = element_control {
                                    let element_control_stream = element_control.into_stream();
                                    log::debug!(
                                        "Spawning element control task for {:?}",
                                        &element_id
                                    );
                                    let element_name = element_name.clone();
                                    Task::local({
                                        let svc = self.clone();
                                        let element_id = element_id.clone();
                                        async move {
                                            if let Err(err) = svc
                                                .run_element_control(
                                                    element_id,
                                                    element_name,
                                                    element_control_stream,
                                                )
                                                .await
                                            {
                                                log::debug!("run_element_control err: {:?}", err);
                                            }
                                        }
                                    })
                                    .detach();
                                }
                                if let Some(lessor_channel) = lessor_channel {
                                    log::debug!("Spawning lessor task for {:?}", &element_id);
                                    let lessor_stream = lessor_channel.into_stream();
                                    Task::local({
                                        let svc = self.clone();
                                        let element_id = element_id.clone();
                                        async move {
                                            if let Err(err) = svc
                                                .run_lessor(
                                                    element_id.clone(),
                                                    element_name.clone(),
                                                    lessor_stream,
                                                )
                                                .await
                                            {
                                                log::debug!(
                                                    "run_lessor({element_id:?}) err: {:?}",
                                                    err
                                                );
                                            }
                                        }
                                    })
                                    .detach();
                                }
                                responder.send(Ok(())).context("send failed")
                            }
                            Err(err) => responder.send(Err(err.into())).context("send failed"),
                        }
                    }
                    TopologyRequest::_UnknownMethod { ordinal, .. } => {
                        log::warn!("Received unknown TopologyRequest: {ordinal}");
                        todo!()
                    }
                }
            })
            .await
    }

    async fn create_status_channel_handler(
        &self,
        element_id: ElementID,
        server_end: ServerEnd<fpb::StatusMarker>,
    ) -> Result<(), Error> {
        let current_level_subscriber =
            self.broker.borrow_mut().new_current_level_subscriber(&element_id);
        let mut handler = StatusChannelHandler::new(element_id.clone());
        let stream = server_end.into_stream();
        handler.start(stream, current_level_subscriber);
        self.element_handlers
            .borrow_mut()
            .entry(element_id.clone())
            .and_modify(|e| e.status.push(handler));
        Ok(())
    }
}

struct ElementRunnerHandler {
    element_id: ElementID,
    element_name: String,
    shutdown: Event,
}

impl ElementRunnerHandler {
    fn new(element_id: ElementID, element_name: String) -> Self {
        Self { element_id, element_name, shutdown: Event::new() }
    }

    fn start(&mut self, broker: Rc<RefCell<Broker>>, element_runner: fpb::ElementRunnerProxy) {
        let element_id = self.element_id.clone();
        let debug_info =
            format!("ElementRunnerHandler<{}:{}>", &self.element_name, &self.element_id);
        // Use a shutdown event to ensure any in progress level transition handshakes are completed
        // before terminating the task.
        let mut shutdown = self.shutdown.wait_or_dropped();
        let mut receiver = broker.borrow_mut().watch_required_level(&element_id);
        log::debug!("{debug_info} starting.");
        Task::local(async move {
            loop {
                select! {
                    _ = shutdown => {
                        break;
                    }
                    required_level = receiver.next() => {
                        match required_level {
                            Some(Some(required_level)) => {
                                log::debug!("{debug_info} calling set_level({required_level:?})");
                                if let Err(err) = element_runner.set_level(required_level.level).await {
                                    log::warn!("{debug_info}: set_level error: {:?}", err);
                                } else {
                                    log::debug!("{debug_info} set_level({required_level:?}) completed.");
                                    broker.borrow_mut().update_current_level(&element_id, required_level);
                                }
                            },
                            None => {
                                log::debug!("{debug_info} receiver closed (element removed)");
                                break;
                            },
                            _ => {
                                log::error!("{debug_info}: unexpected required_level: {:?}", required_level);
                            }
                        }
                    }
                }
            }
            log::debug!("{debug_info} shutdown.");
        }).detach();
    }
}

struct StatusChannelHandler {
    element_id: ElementID,
    shutdown: Event,
}

impl StatusChannelHandler {
    fn new(element_id: ElementID) -> Self {
        Self { element_id, shutdown: Event::new() }
    }

    fn start(&mut self, mut stream: StatusRequestStream, subscriber: CurrentLevelSubscriber) {
        let element_id = self.element_id.clone();
        let mut shutdown = self.shutdown.wait_or_dropped();
        log::debug!("Starting new StatusChannelHandler for {:?}", &self.element_id);
        Task::local(async move {
            let subscriber = subscriber;
            loop {
                select! {
                    _ = shutdown => {
                        break;
                    }
                    next = stream.next() => {
                        if let Some(Ok(request)) = next {
                            if let Err(err) = StatusChannelHandler::handle_request(request, &subscriber).await {
                                log::debug!("handle_request error: {:?}", err);
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
            log::debug!("Closed StatusChannel for {:?}.", &element_id);
        }).detach();
    }

    async fn handle_request(
        request: StatusRequest,
        subscriber: &CurrentLevelSubscriber,
    ) -> Result<(), Error> {
        match request {
            StatusRequest::WatchPowerLevel { responder } => {
                subscriber.register(responder)?;
                Ok(())
            }
            StatusRequest::_UnknownMethod { ordinal, .. } => {
                log::warn!("Received unknown StatusRequest: {ordinal}");
                todo!()
            }
        }
    }
}

#[fuchsia::main(logging = true)]
async fn main() -> Result<(), anyhow::Error> {
    let mut service_fs = ServiceFs::new_local();

    fuchsia_trace_provider::trace_provider_create_with_fdio();

    // Initialize inspect
    let _inspect_server = inspect_runtime::publish(
        // TODO(https://fxbug.dev/354754310): reduce size if possible
        component::init_inspector_with_size(9 * DEFAULT_INSPECT_VMO),
        inspect_runtime::PublishOptions::default(),
    );
    component::serve_inspect_stats();
    component::health().set_starting_up();

    service_fs.dir("svc").add_fidl_service(IncomingRequest::Topology);

    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;

    component::health().set_ok();

    let svc = Rc::new(BrokerSvc::new());

    service_fs
        .for_each_concurrent(None, |request: IncomingRequest| async {
            match request {
                IncomingRequest::Topology(stream) => {
                    svc.clone().run_topology(stream).await.expect("run_topology failed");
                }
            }
            ()
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[fuchsia::test]
    async fn smoke_test() {
        assert!(true);
    }
}
