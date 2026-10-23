// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use async_utils::event::Event;
use fidl::endpoints::{ClientEnd, ServerEnd};
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_hardware_power_statecontrol as fstatecontrol;
use fidl_fuchsia_power_broker::{
    self as fpb, ElementControlRequest, ElementControlRequestStream, LeaseControlMarker,
    LeaseControlRequest, LeaseControlRequestStream, LeaseError, LeaseStatus, LessorRequest,
    LessorRequestStream, StatusRequest, StatusRequestStream, TopologyRequest,
    TopologyRequestStream,
};
use fpb::ElementSchema;
use fuchsia_async::{MonotonicInstant, Task, Timer};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use futures::prelude::*;
use futures::select;
use inspect_format::constants::DEFAULT_VMO_SIZE_BYTES as DEFAULT_INSPECT_VMO;
use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::pin;
use std::rc::Rc;
use zx::{Duration, Peered};

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
    stuck_detector: StuckDetector,
}

impl BrokerSvc {
    fn new(stuck_detector: StuckDetector) -> Self {
        Self {
            broker: Rc::new(RefCell::new(Broker::new(
                component::inspector().root().create_child("broker"),
            ))),
            element_handlers: Rc::new(RefCell::new(HashMap::new())),
            stuck_detector,
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
                let debug_info = format!("Lessor<{}:{}>", element_name, element_id);
                match request {
                    LessorRequest::Lease { level, responder } => {
                        fuchsia_trace::duration!(c"power-broker", c"Lessor::Lease");
                        log::debug!("{debug_info}: Leasing @ {level}");
                        let (client, server_end) =
                            fidl::endpoints::create_endpoints::<LeaseControlMarker>();
                        let lease_control_koid =
                            server_end.channel().as_handle_ref().koid().unwrap();
                        let resp = {
                            let mut broker = self.broker.borrow_mut();
                            let Some(level) =
                                broker.get_level_index(element_id, &level).map(|l| l.clone())
                            else {
                                return responder
                                    .send(Err(LeaseError::InvalidLevel))
                                    .context("send failed");
                            };
                            broker.acquire_lease(element_id, level, lease_control_koid)
                        };
                        match resp {
                            Ok(lease) => {
                                log::debug!("{debug_info}: Lease granted: {lease:?}");
                                let lease_id = lease.id;
                                log::debug!(
                                    "{debug_info}: Spawning lease control task for {lease_id}");
                                Task::local({
                                    let svc = self.clone();
                                    async move {
                                        if let Err(err) = svc
                                            .run_lease_control(lease.id, server_end.into_stream())
                                            .await
                                        {
                                            log::debug!("{debug_info}: run_lease_control err: {:?}", err);
                                        }
                                        // When the channel is closed, drop the lease if it is still
                                        // present (it may have already been dropped if its target
                                        // element was removed).
                                        log::debug!(
                                            "{debug_info}: Channel closed, dropping lease {lease_id}"
                                        );
                                        let mut broker = svc.broker.borrow_mut();
                                        if let Err(err) = broker.drop_lease(lease.id) {
                                            log::debug!("{debug_info}: drop_lease {lease_id} failed: {:?}", err);
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
        lease_id: LeaseID,
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
                        fuchsia_trace::duration!(c"power-broker", c"LeaseControl::WatchStatus");
                        log::debug!(
                            "WatchStatus({:?}, {:?})",
                            lease_id,
                            last_status
                        );
                        let mut receiver = {
                            let mut broker = self.broker.borrow_mut();
                            broker.watch_lease_status(lease_id)
                        };
                        while let Some(next) = receiver.next().await {
                            log::debug!(
                                "receiver.next = {:?}, last_status = {:?}",
                                next,
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
                                    "WatchStatus: sending new status: {:?}", status,
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

    async fn handle_direct_lease(
        self: Rc<Self>,
        lease_token: zx::EventPair,
        lease_name: String,
        dependencies: Vec<fpb::LeaseDependency>,
        should_return_pending_lease: bool,
    ) -> Result<(), fpb::LeaseError> {
        let lease_token_koid = lease_token.as_handle_ref().koid().unwrap();
        let resp = {
            let mut broker = self.broker.borrow_mut();
            broker.acquire_direct_lease(lease_name, dependencies, lease_token_koid)
        };
        match resp {
            Ok(lease) => {
                log::debug!("Direct lease granted: {lease:?}");
                if let Err(err) = self.clone().monitor_lease(lease.id, lease_token) {
                    log::error!("monitor_lease failed: {:?}", err);
                    return Err(fpb::LeaseError::Internal);
                }
                let lease_id = lease.id;

                if !should_return_pending_lease {
                    let mut receiver = {
                        let mut broker = self.broker.borrow_mut();
                        broker.watch_lease_status(lease_id)
                    };
                    while let Some(status) = receiver.next().await {
                        if status == Some(LeaseStatus::Satisfied) {
                            break;
                        }
                    }
                }
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
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
                    element_id,
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
        element_id: ElementID,
        element_name: String,
        request: ElementControlRequest,
    ) -> Result<(), Error> {
        let debug_info = format!("ElementControl<{}:{}>", element_name, element_id);
        match request {
            ElementControlRequest::OpenStatusChannel { status_channel, .. } => {
                fuchsia_trace::duration!(c"power-broker", c"ElementControl::OpenStatusChannel");
                log::debug!("{debug_info}: OpenStatusChannel");
                let svc = self.clone();
                svc.create_status_channel_handler(element_id, status_channel).await
            }
            ElementControlRequest::RegisterDependencyToken { token, responder, .. } => {
                fuchsia_trace::duration!(
                    c"power-broker",
                    c"ElementControl::RegisterDependencyToken"
                );
                log::debug!("{debug_info}: RegisterDependencyToken({token:?})");
                let mut broker = self.broker.borrow_mut();
                let res = broker.register_dependency_token(element_id, token.into());
                log::debug!(
                    "{debug_info}: RegisterDependencyToken register_credentials = ({res:?})"
                );
                responder.send(res.map_err(Into::into)).context("send failed")
            }
            ElementControlRequest::UnregisterDependencyToken { token, responder } => {
                fuchsia_trace::duration!(
                    c"power-broker",
                    c"ElementControl::UnregisterDependencyToken"
                );
                log::debug!("{debug_info}: UnregisterDependencyToken({token:?})");
                let mut broker = self.broker.borrow_mut();
                let res = broker.unregister_dependency_token(element_id, token.into());
                log::debug!(
                    "{debug_info}: UnregisterDependencyToken unregister_credentials = ({res:?})"
                );
                responder.send(res.map_err(Into::into)).context("send failed")
            }
            ElementControlRequest::AddDependency { payload, responder } => {
                fuchsia_trace::duration!(c"power-broker", c"ElementControl::AddDependency");
                let dep = payload;
                log::debug!(
                    "{debug_info}: AddDependency({:?}, {:?}, {:?}, {:?})",
                    dep.dependent_level,
                    dep.requires_token,
                    dep.requires_level_by_preference,
                    dep.remove_with_required_element
                );
                let provisional_lease_id =
                    match self.broker.borrow_mut().prepare_add_dependency(element_id, &dep) {
                        Ok(res) => res.map(|l| l.id),
                        Err(err) => {
                            responder.send(Err(err.into())).context("send failed")?;
                            return Ok(());
                        }
                    };

                if let Some(lease_id) = provisional_lease_id {
                    let mut rx = self.broker.borrow_mut().watch_lease_status(lease_id);
                    while let Some(status) = rx.next().await {
                        if status == Some(LeaseStatus::Satisfied) {
                            break;
                        }
                    }
                }
                let res = self.broker.borrow_mut().add_dependency(
                    element_id,
                    dep,
                    &mut crate::inspect::EagerInspectWriter,
                );

                // Drop provisional lease (if one was taken; it may have already been dropped if
                // the required element was removed while waiting for the lease to be satisfied).
                if let Some(lease_id) = provisional_lease_id {
                    if let Err(err) = self.broker.borrow_mut().drop_lease(lease_id) {
                        log::debug!("{debug_info}: drop_lease {lease_id} failed: {:?}", err);
                    }
                }
                responder.send(res.map_err(Into::into)).context("send failed")?;
                Ok(())
            }
            ElementControlRequest::_UnknownMethod { ordinal, .. } => {
                log::warn!("{debug_info}: Received unknown ElementControlRequest: {ordinal}");
                todo!()
            }
        }
    }

    /// Spawns background tasks to monitor a lease token for two events:
    /// 1. If the client closes their end of the token, the lease should be dropped.
    /// 2. When the lease is satisfied, `LEASE_SIGNAL_SATISFIED` should be signaled.
    fn monitor_lease(
        self: Rc<Self>,
        lease_id: LeaseID,
        lease_token: zx::EventPair,
    ) -> Result<(), Error> {
        let lease_token_signal =
            lease_token.duplicate_handle(zx::Rights::SAME_RIGHTS).context("dup handle failed")?;
        // Task to drop the lease when the token is closed.
        Task::local({
            let svc = self.clone();
            let lease_id = lease_id;
            async move {
                let _ =
                    fuchsia_async::OnSignals::new(&lease_token, zx::Signals::EVENTPAIR_PEER_CLOSED)
                        .await;
                log::debug!("LeaseToken closed, dropping lease {lease_id}");
                let mut broker = svc.broker.borrow_mut();
                if let Err(err) = broker.drop_lease(lease_id) {
                    log::debug!("drop_lease {lease_id} failed: {:?}", err);
                }
            }
        })
        .detach();

        // Task to signal LEASE_SIGNAL_SATISFIED when satisfied.
        Task::local({
            let lease_id = lease_id;
            let broker = self.broker.clone();
            async move {
                let mut receiver = {
                    let mut broker = broker.borrow_mut();
                    broker.watch_lease_status(lease_id)
                };
                while let Some(next) = receiver.next().await {
                    let (clear_mask, set_mask) = if let Some(LeaseStatus::Satisfied) = next {
                        (
                            zx::Signals::NONE,
                            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                        )
                    } else {
                        (
                            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                            zx::Signals::NONE,
                        )
                    };
                    if let Err(err) = lease_token_signal.signal_peer(clear_mask, set_mask) {
                        log::debug!("signal_peer failed for lease {lease_id}: {:?}", err);
                        break;
                    }
                }
            }
        })
        .detach();
        Ok(())
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
            Option<fpb::LeaseToken>,
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
            payload.initial_lease_token,
        ))
    }

    fn validate_and_unpack_lease_payload(
        payload: fpb::LeaseSchema,
    ) -> Result<(zx::EventPair, String, Vec<fpb::LeaseDependency>, bool), fpb::LeaseError> {
        let Some(lease_token) = payload.lease_token else {
            return Err(fpb::LeaseError::InvalidArgument);
        };
        let Some(lease_name) = payload.lease_name else {
            return Err(fpb::LeaseError::InvalidArgument);
        };
        let Some(dependencies) = payload.dependencies else {
            return Err(fpb::LeaseError::InvalidArgument);
        };
        // A lease with no dependencies would have no targets and no claims, so it would be
        // trivially satisfied and invisible in Inspect.
        if dependencies.is_empty() {
            return Err(fpb::LeaseError::InvalidArgument);
        }
        for dep in &dependencies {
            if dep.requires_level.is_none() && dep.requires_level_by_preference.is_none() {
                return Err(fpb::LeaseError::InvalidLevel);
            }
        }
        let should_return_pending_lease = payload.should_return_pending_lease.unwrap_or(false);
        Ok((lease_token, lease_name, dependencies, should_return_pending_lease))
    }

    async fn run_topology(self: Rc<Self>, stream: TopologyRequestStream) -> Result<(), Error> {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each(|request| async {
                match request {
                    TopologyRequest::AddElement { payload, responder } => {
                        fuchsia_trace::duration!(c"power-broker", c"Topology::AddElement");
                        log::debug!("AddElement({:?})", payload);
                        let Ok((
                            element_name,
                            initial_current_level,
                            valid_levels,
                            level_dependencies,
                            lessor_channel,
                            element_control,
                            element_runner,
                            initial_lease_token,
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
                                    .insert(element_id, ElementHandlers::new());

                                if let Some(element_control) = element_control {
                                    let element_control_stream = element_control.into_stream();
                                    log::debug!(
                                        "Spawning element control task for {:?}",
                                        element_id
                                    );
                                    let element_name = element_name.clone();
                                    Task::local({
                                        let svc = self.clone();
                                        let element_id = element_id;
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
                                    log::debug!("Spawning lessor task for {:?}", element_id);
                                    let lessor_stream = lessor_channel.into_stream();
                                    let element_name = element_name.clone();
                                    Task::local({
                                        let svc = self.clone();
                                        let element_id = element_id;
                                        async move {
                                            if let Err(err) = svc
                                                .run_lessor(
                                                    element_id,
                                                    element_name,
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

                                let runner_lease_token: Option<fpb::LeaseToken> = {
                                    initial_lease_token
                                        .as_ref()
                                        .and_then(|t| t.duplicate_handle(zx::Rights::SAME_RIGHTS).ok())
                                };

                                if let Some(lease_token) = initial_lease_token {
                                    let lease_token_koid =
                                        lease_token.as_handle_ref().koid().unwrap();

                                    let resp = {
                                        let mut broker = self.broker.borrow_mut();
                                        match broker.get_level_index(element_id, &initial_current_level) {
                                            Some(lvl) => {
                                                let level = lvl.clone();
                                                broker
                                                    .acquire_lease(element_id, level, lease_token_koid)
                                                    .map_err(|e| match e {
                                                        fpb::LeaseError::NotAuthorized => {
                                                            fpb::AddElementError::NotAuthorized
                                                        }
                                                        _ => fpb::AddElementError::Invalid,
                                                    })
                                            }
                                            None => Err(fpb::AddElementError::Invalid),
                                        }
                                    };

                                    match resp {
                                        Ok(lease) => {
                                            log::debug!(
                                                "Initial lease granted for {element_id:?}: {lease:?}"
                                            );
                                            if let Err(err) = self.clone().monitor_lease(
                                                lease.id,
                                                lease_token,
                                            ) {
                                                log::error!("Failed to monitor lease: {err:?}");
                                                return responder
                                                    .send(Err(fpb::AddElementError::Invalid))
                                                    .context("send failed");
                                            }
                                        }
                                        Err(err) => {
                                            log::error!("Failed to lease element {element_id}: {err:?}");
                                            return responder
                                                .send(Err(err))
                                                .context("send failed");
                                        }
                                    }
                                }

                                let wait_for_level = if runner_lease_token.is_some() {
                                    self.broker
                                        .borrow()
                                        .get_level_index(element_id, &initial_current_level)
                                        .cloned()
                                } else {
                                    None
                                };

                                let mut runner = ElementRunnerHandler::new(
                                    element_id,
                                    element_name,
                                    wait_for_level,
                                    runner_lease_token,
                                );
                                runner.start(
                                    self.broker.clone(),
                                    element_runner.into_proxy(),
                                    self.stuck_detector.clone(),
                                );
                                self.element_handlers
                                    .borrow_mut()
                                    .entry(element_id)
                                    .and_modify(|e| {
                                        e.runner = Some(runner);
                                    });

                                responder.send(Ok(())).context("send failed")
                            }
                            Err(err) => responder.send(Err(err.into())).context("send failed"),
                        }
                    }
                    TopologyRequest::Lease { payload, responder } => {
                        fuchsia_trace::duration!(c"power-broker", c"Topology::Lease");
                        log::debug!("Lease({:?})", payload);
                        let (
                            lease_token,
                            lease_name,
                            dependencies,
                            should_return_pending_lease,
                        ) = match Self::validate_and_unpack_lease_payload(payload) {
                            Ok(payload) => payload,
                            Err(err) => {
                                return responder.send(Err(err)).context("send failed");
                            }
                        };
                        let res = self
                            .clone()
                            .handle_direct_lease(
                                lease_token,
                                lease_name,
                                dependencies,
                                should_return_pending_lease,
                            )
                            .await;
                        responder.send(res).context("send failed")
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
            self.broker.borrow_mut().new_current_level_subscriber(element_id);
        let mut handler = StatusChannelHandler::new(element_id);
        let stream = server_end.into_stream();
        handler.start(stream, current_level_subscriber);
        self.element_handlers.borrow_mut().entry(element_id).and_modify(|e| e.status.push(handler));
        Ok(())
    }
}

const DEFAULT_WATCHDOG_INTERVAL_SECONDS: u32 = 10;

#[derive(Clone)]
struct StuckDetector {
    set_level_timeout_seconds: u32,
    reboot_on_set_level_timeout: bool,
    crash_reporter: Option<ffeedback::CrashReporterProxy>,
    admin: Option<fstatecontrol::AdminProxy>,
}

impl StuckDetector {
    fn new(
        set_level_timeout_seconds: u32,
        reboot_on_set_level_timeout: bool,
        crash_reporter: Option<ffeedback::CrashReporterProxy>,
        admin: Option<fstatecontrol::AdminProxy>,
    ) -> Self {
        if reboot_on_set_level_timeout && set_level_timeout_seconds == 0 {
            log::error!(
                "reboot_on_set_level_timeout is true, but set_level_timeout_seconds is 0; reboot will not occur on timeout"
            );
        }
        if set_level_timeout_seconds > 0 && crash_reporter.is_none() {
            log::error!(
                "set_level_timeout_seconds is {}, but crash_reporter is None; crash reports will not be filed",
                set_level_timeout_seconds
            );
        }
        if set_level_timeout_seconds > 0 && reboot_on_set_level_timeout && admin.is_none() {
            log::error!(
                "reboot_on_set_level_timeout is true, but admin is None; reboot will not occur on timeout"
            );
        }

        Self { set_level_timeout_seconds, reboot_on_set_level_timeout, crash_reporter, admin }
    }

    fn from_config(config: &broker_config::Config) -> Self {
        let crash_reporter = if config.set_level_timeout_seconds > 0 {
            fuchsia_component::client::connect_to_protocol::<ffeedback::CrashReporterMarker>().ok()
        } else {
            None
        };

        let admin = if config.set_level_timeout_seconds > 0 && config.reboot_on_set_level_timeout {
            fuchsia_component::client::connect_to_protocol::<fstatecontrol::AdminMarker>().ok()
        } else {
            None
        };

        Self::new(
            config.set_level_timeout_seconds,
            config.reboot_on_set_level_timeout,
            crash_reporter,
            admin,
        )
    }

    fn spawn_watchdog(
        &self,
        element_name: String,
        debug_info: String,
        required_level: IndexedPowerLevel,
    ) -> Task<()> {
        let this = self.clone();
        Task::local(async move {
            let mut crash_reported = false;
            let mut elapsed_s = 0u32;
            loop {
                let next_interval_s = if this.set_level_timeout_seconds > 0
                    && elapsed_s < this.set_level_timeout_seconds
                {
                    std::cmp::min(
                        DEFAULT_WATCHDOG_INTERVAL_SECONDS,
                        this.set_level_timeout_seconds - elapsed_s,
                    )
                } else {
                    DEFAULT_WATCHDOG_INTERVAL_SECONDS
                };

                Timer::new(MonotonicInstant::after(Duration::from_seconds(next_interval_s as i64)))
                    .await;
                elapsed_s += next_interval_s;

                log::warn!(
                    "{debug_info}: set_level({required_level:?}) still pending after {elapsed_s}s..."
                );

                if this.set_level_timeout_seconds > 0
                    && elapsed_s >= this.set_level_timeout_seconds
                    && !crash_reported
                {
                    log::warn!(
                        "{debug_info}: set_level({required_level:?}) timed out after {elapsed_s}s! Filing crash report..."
                    );

                    crash_reported = true;
                    let reporter = this.crash_reporter.clone();
                    let admin = this.admin.clone();
                    let reboot_on_timeout = this.reboot_on_set_level_timeout;
                    let element_name = element_name.clone();

                    // Spawn reporting and reboot onto the local executor so it runs to
                    // completion even if set_level returns and drops _warn_task.
                    Task::local(async move {
                        if let Some(reporter) = reporter {
                            let report = ffeedback::CrashReport {
                                program_name: Some("power-broker".to_string()),
                                crash_signature: Some(format!(
                                    "fuchsia-power-broker-set-level-stuck-{element_name}"
                                )),
                                is_fatal: Some(false),
                                ..Default::default()
                            };
                            match reporter.file_report(report).await {
                                Ok(Ok(result)) => log::info!("Crash report filed: {:?}", result),
                                Ok(Err(e)) => log::warn!("Failed to file crash report: {:?}", e),
                                Err(e) => log::warn!("Failed to call FileReport: {:?}", e),
                            }
                        }

                        if reboot_on_timeout {
                            if let Some(admin) = admin {
                                log::info!("Initiating reboot due to stuck SetLevel...");
                                let options = fstatecontrol::ShutdownOptions {
                                    action: Some(fstatecontrol::ShutdownAction::Reboot),
                                    reasons: Some(vec![
                                        fstatecontrol::ShutdownReason::PowerLevelTransitionStuck,
                                    ]),
                                    ..Default::default()
                                };
                                match admin.shutdown(&options).await {
                                    Ok(Ok(())) => {
                                        log::info!("Shutdown/Reboot requested successfully")
                                    }
                                    Ok(Err(e)) => log::error!("Shutdown/Reboot failed: {:?}", e),
                                    Err(e) => log::error!("Failed to call Shutdown: {:?}", e),
                                }
                            }
                        }
                    })
                    .detach();
                }
            }
        })
    }
}

struct ElementRunnerHandler {
    element_id: ElementID,
    element_name: String,
    shutdown: Event,
    wait_for_level: Option<IndexedPowerLevel>,
    initial_lease_token: Option<zx::EventPair>,
}

impl ElementRunnerHandler {
    fn new(
        element_id: ElementID,
        element_name: String,
        wait_for_level: Option<IndexedPowerLevel>,
        initial_lease_token: Option<zx::EventPair>,
    ) -> Self {
        Self {
            element_id,
            element_name,
            shutdown: Event::new(),
            wait_for_level,
            initial_lease_token,
        }
    }

    fn start(
        &mut self,
        broker: Rc<RefCell<Broker>>,
        element_runner: fpb::ElementRunnerProxy,
        stuck_detector: StuckDetector,
    ) {
        let element_id = self.element_id;
        let element_name = self.element_name.clone();
        let debug_info = format!("ElementRunnerHandler<{}:{}>", self.element_name, self.element_id);
        // Use a shutdown event to ensure any in progress level transition handshakes are completed
        // before terminating the task.
        let mut shutdown = self.shutdown.wait_or_dropped();
        let mut receiver = broker.borrow_mut().watch_required_level(&element_id);
        let mut wait_for_level = self.wait_for_level.clone();
        let mut suppressed_level: Option<IndexedPowerLevel> = None;
        let initial_lease_token = self.initial_lease_token.take();
        log::debug!("{debug_info} starting.");
        Task::local(async move {
            let initial_lease_fut = match &initial_lease_token {
                Some(token) => futures::future::Either::Left(
                    fuchsia_async::OnSignals::new(token, zx::Signals::EVENTPAIR_PEER_CLOSED).fuse()
                ),
                None => futures::future::Either::Right(futures::future::pending()),
            };
            let mut initial_lease_fut = pin!(initial_lease_fut);

            // Listen for updates from the broker on the Element's required level, and issue
            // corresponding SetLevel calls to the ElementRunner. Continues until shutdown is
            // signaled, presumably because the Element has been removed from the topology.
            loop {
                select! {
                    _ = shutdown => {
                        break;
                    }
                    // If an initial lease is dropped before being satisfied, we need to send a
                    // SetLevel call for the most recent required level that we suppressed.
                    _ = &mut initial_lease_fut => {
                        log::debug!("{debug_info}: initial lease dropped by client. Clearing wait_for_level.");
                        wait_for_level = None;
                        if let Some(level) = suppressed_level.take() {
                            log::debug!("{debug_info}: emitting previously suppressed level {:?}", level);
                            let _warn_task = stuck_detector.spawn_watchdog(
                                element_name.clone(),
                                debug_info.clone(),
                                level.clone(),
                            );
                            fuchsia_trace::duration!("power-broker", "ElementRunner::SetLevel", "element_name" => element_name.as_str());
                            if let Err(err) = element_runner.set_level(level.level).await {
                                log::warn!("{debug_info}: set_level error during suppressed level: {:?}", err);
                            } else {
                                broker.borrow_mut().update_current_level(element_id, level);
                            }
                        }
                    }
                    required_level = receiver.next() => {
                        match required_level {
                            Some(Some(required_level)) => {
                                // While an initial_lease is pending, suppress any SetLevel calls
                                // that are for a lower power level than the one we are waiting for.
                                // This allows us to support adding an element with an
                                // initial_current_level above the minimum level.
                                if let Some(wait) = &wait_for_level {
                                    if required_level.level < wait.level {
                                       // We must update the broker state to complete the transition,
                                       // otherwise the broker will block future updates (like going back to 1).
                                       suppressed_level = Some(required_level.clone());
                                       broker.borrow_mut().update_current_level(element_id, required_level.clone());
                                       continue;
                                    } else {
                                        wait_for_level = None;
                                        suppressed_level = None;
                                    }
                                }
                                log::debug!("{debug_info} calling set_level({required_level:?})");
                                let _warn_task = stuck_detector.spawn_watchdog(
                                    element_name.clone(),
                                    debug_info.clone(),
                                    required_level.clone(),
                                );

                                fuchsia_trace::duration!("power-broker", "ElementRunner::SetLevel", "element_name" => element_name.as_str());

                                if let Err(err) = element_runner.set_level(required_level.level).await {
                                    log::warn!("{debug_info}: set_level error: {:?}", err);
                                } else {
                                    log::debug!("{debug_info} set_level({required_level:?}) completed.");
                                    broker.borrow_mut().update_current_level(element_id, required_level);
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
        let element_id = self.element_id;
        let mut shutdown = self.shutdown.wait_or_dropped();
        log::debug!("Starting new StatusChannelHandler for {:?}", self.element_id);
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
            log::debug!("Closed StatusChannel for {:?}.", element_id);
        }).detach();
    }

    async fn handle_request(
        request: StatusRequest,
        subscriber: &CurrentLevelSubscriber,
    ) -> Result<(), Error> {
        fuchsia_trace::duration!(c"power-broker", c"StatusChannel::handle_request");
        match request {
            StatusRequest::WatchPowerLevel { responder } => {
                fuchsia_trace::duration!(c"power-broker", c"StatusRequest::WatchPowerLevel");
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
    let config = broker_config::Config::take_from_startup_handle();

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

    log::info!(
        "StuckDetector config: set_level_timeout_seconds={}, reboot_on_set_level_timeout={}",
        config.set_level_timeout_seconds,
        config.reboot_on_set_level_timeout
    );
    let stuck_detector = StuckDetector::from_config(&config);
    let svc = Rc::new(BrokerSvc::new(stuck_detector));

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
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use test_case::test_case;

    #[test_case(false; "no_reboot")]
    #[test_case(true; "reboot")]
    #[fuchsia::test]
    fn test_stuck_detector(reboot_enabled: bool) {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (crash_reporter_proxy, mut crash_reporter_stream) =
            create_proxy_and_stream::<ffeedback::CrashReporterMarker>();
        let (admin_proxy, mut admin_stream) =
            create_proxy_and_stream::<fstatecontrol::AdminMarker>();

        let detector =
            StuckDetector::new(1, reboot_enabled, Some(crash_reporter_proxy), Some(admin_proxy));

        let task = detector.spawn_watchdog(
            "test_element".to_string(),
            "test_debug".to_string(),
            IndexedPowerLevel { level: 1, index: 1 },
        );

        // Run until stalled to start the watchdog task and register the timer.
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        assert!(crash_reporter_stream.next().now_or_never().is_none());

        // Advance fake time by 1s to trigger the timeout and crash report.
        executor.set_fake_time(executor.now() + Duration::from_seconds(1));
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Check crash report.
        let ffeedback::CrashReporterRequest::FileReport { report, responder } =
            crash_reporter_stream
                .next()
                .now_or_never()
                .expect("Expected crash report")
                .expect("Crash report stream closed")
                .unwrap();
        assert_eq!(report.program_name, Some("power-broker".to_string()));
        assert_eq!(
            report.crash_signature,
            Some("fuchsia-power-broker-set-level-stuck-test_element".to_string())
        );
        assert_eq!(report.is_fatal, Some(false));
        responder.send(Ok(&ffeedback::FileReportResults::default())).unwrap();
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Check reboot.
        if reboot_enabled {
            let req = admin_stream
                .next()
                .now_or_never()
                .expect("Expected reboot request")
                .expect("Admin stream closed")
                .unwrap();
            let fstatecontrol::AdminRequest::Shutdown { options, responder } = req else {
                panic!("Expected Shutdown request, got {:?}", req);
            };
            assert_eq!(options.action, Some(fstatecontrol::ShutdownAction::Reboot));
            assert_eq!(
                options.reasons,
                Some(vec![fstatecontrol::ShutdownReason::PowerLevelTransitionStuck])
            );
            responder.send(Ok(())).unwrap();
            let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        } else {
            // Verify that NO reboot request was received.
            assert!(admin_stream.next().now_or_never().is_none());
        }

        // Clean up task.
        drop(task);
    }

    #[test_case(false; "no_reboot")]
    #[test_case(true; "reboot")]
    #[fuchsia::test]
    fn test_stuck_detector_disabled(reboot_enabled: bool) {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (crash_reporter_proxy, mut crash_reporter_stream) =
            create_proxy_and_stream::<ffeedback::CrashReporterMarker>();
        let (admin_proxy, mut admin_stream) =
            create_proxy_and_stream::<fstatecontrol::AdminMarker>();

        let detector =
            StuckDetector::new(0, reboot_enabled, Some(crash_reporter_proxy), Some(admin_proxy));

        let task = detector.spawn_watchdog(
            "test_element".to_string(),
            "test_debug".to_string(),
            IndexedPowerLevel { level: 1, index: 1 },
        );

        // Run until stalled to start the watchdog task and register the timer.
        let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());

        // Advance fake time past multiple watchdog intervals (60s total).
        for _ in 0..6 {
            executor.set_fake_time(
                executor.now() + Duration::from_seconds(DEFAULT_WATCHDOG_INTERVAL_SECONDS.into()),
            );
            let _ = executor.run_until_stalled(&mut futures::future::pending::<()>());
        }

        // Verify that NO crash report was filed and NO reboot was requested.
        assert!(crash_reporter_stream.next().now_or_never().is_none());
        assert!(admin_stream.next().now_or_never().is_none());

        drop(task);
    }

    #[fuchsia::test]
    async fn test_add_dependency_required_element_removed_while_provisional_lease_pending() {
        let svc = Rc::new(BrokerSvc::new(StuckDetector::new(0, false, None, None)));
        let token_a = fpb::DependencyToken::create();

        let (element_a, element_b) = {
            let mut broker = svc.broker.borrow_mut();
            let element_a = broker.add_element("A", 0, vec![0, 1], vec![]).unwrap();
            broker
                .register_dependency_token(
                    element_a,
                    token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                )
                .unwrap();
            let element_b = broker.add_element("B", 0, vec![0, 1], vec![]).unwrap();
            let level_1 = broker.get_level_index(element_b, &1).unwrap().clone();
            // Lease B at level 1 so B's required level is 1; adding a dependency from B(1) to A(1)
            // will therefore acquire a provisional lease on A(1) and wait for it to be satisfied.
            let _lease_b = broker.acquire_lease(element_b, level_1, zx::Koid::from_raw(1)).unwrap();
            (element_a, element_b)
        };

        let (element_control_proxy, element_control_stream) =
            create_proxy_and_stream::<fpb::ElementControlMarker>();
        let control_task = Task::local(svc.clone().run_element_control(
            element_b,
            "B".to_string(),
            element_control_stream,
        ));

        let mut add_dep_fut = element_control_proxy.add_dependency(fpb::LevelDependency {
            dependent_level: Some(1),
            requires_token: Some(token_a),
            requires_level_by_preference: Some(vec![1]),
            ..Default::default()
        });

        // Poll once so `handle_element_control_request` acquires the provisional lease on A(1)
        // and suspends waiting for it to reach `LeaseStatus::Satisfied`.
        assert!(futures::poll!(&mut add_dep_fut).is_pending());

        // Remove A while the provisional lease on A(1) is still pending. This drops and vacates
        // the provisional lease and closes its status channel.
        svc.broker.borrow_mut().remove_element(&element_a);

        // `AddDependency` must return an error without panicking when attempting to drop the
        // already-dropped provisional lease.
        let res = add_dep_fut.await.expect("FIDL call should not fail with peer closed");
        assert_eq!(res, Err(fpb::ModifyDependencyError::NotAuthorized));

        drop(element_control_proxy);
        control_task.await.expect("run_element_control failed");
    }
}
