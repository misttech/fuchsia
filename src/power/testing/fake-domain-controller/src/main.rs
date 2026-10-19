// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_cpu as fcpu;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryStreamExt};
use log::info;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::rc::Rc;

struct DomainState {
    frequencies_hz: Vec<u64>,
    legacy_max_lease: Option<fbroker::LeaseToken>,
    max_leases: Vec<(u64, fbroker::LeaseToken)>,
    min_leases: Vec<(u64, fbroker::LeaseToken)>,
}

impl DomainState {
    fn new(frequencies_hz: Vec<u64>) -> Self {
        Self {
            frequencies_hz,
            legacy_max_lease: None,
            max_leases: Vec::new(),
            min_leases: Vec::new(),
        }
    }

    fn prune_closed(&mut self) {
        let is_open = |token: &fbroker::LeaseToken| {
            token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
                .is_err()
        };
        self.max_leases.retain(|(_, token)| is_open(token));
        self.min_leases.retain(|(_, token)| is_open(token));
    }

    fn lease_max(&mut self, frequency_index: u64) -> Option<fbroker::LeaseToken> {
        if frequency_index >= self.frequencies_hz.len() as u64 {
            return None;
        }
        let (client_token, server_token) = fbroker::LeaseToken::create();
        self.max_leases.push((frequency_index, server_token));
        Some(client_token)
    }

    fn lease_min(&mut self, frequency_index: u64) -> Option<fbroker::LeaseToken> {
        if frequency_index >= self.frequencies_hz.len() as u64 {
            return None;
        }
        let (client_token, server_token) = fbroker::LeaseToken::create();
        self.min_leases.push((frequency_index, server_token));
        Some(client_token)
    }

    fn effective_max_opp(&mut self) -> u64 {
        self.prune_closed();
        self.max_leases
            .iter()
            .map(|(opp, _)| *opp)
            .min_by_key(|&idx| (self.frequencies_hz[idx as usize], Reverse(idx)))
            .unwrap_or(0)
    }

    fn effective_min_opp(&mut self) -> u64 {
        self.prune_closed();
        let default_min_opp = (self.frequencies_hz.len() - 1) as u64;
        self.min_leases
            .iter()
            .map(|(opp, _)| *opp)
            .max_by_key(|&idx| (self.frequencies_hz[idx as usize], Reverse(idx)))
            .unwrap_or(default_min_opp)
    }
}

fn handle_domain_controller_stream(
    mut stream: fcpu::DomainControllerRequestStream,
    domains: Rc<Vec<fcpu::DomainInfo>>,
    domain_states: Rc<RefCell<HashMap<u64, DomainState>>>,
) {
    fasync::Task::local(async move {
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                fcpu::DomainControllerRequest::ListDomains { responder } => {
                    responder.send(domains.as_ref()).unwrap();
                }
                fcpu::DomainControllerRequest::GetMaxFrequency { domain_id, responder } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    responder.send(Ok(state.effective_max_opp())).unwrap();
                }
                fcpu::DomainControllerRequest::SetMaxFrequency {
                    domain_id,
                    frequency_index,
                    responder,
                } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    if let Some(token) = state.lease_max(frequency_index) {
                        state.legacy_max_lease = Some(token);
                        responder.send(Ok(())).unwrap();
                    } else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                    }
                }
                fcpu::DomainControllerRequest::ClearMaxFrequency { domain_id, responder } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    state.legacy_max_lease = None;
                    responder.send(Ok(())).unwrap();
                }
                fcpu::DomainControllerRequest::GetMinFrequency { domain_id, responder } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    responder.send(Ok(state.effective_min_opp())).unwrap();
                }
                fcpu::DomainControllerRequest::LeaseMaxFrequency {
                    domain_id,
                    frequency_index,
                    lease_name: _,
                    responder,
                } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    if let Some(token) = state.lease_max(frequency_index) {
                        responder.send(Ok(token)).unwrap();
                    } else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                    }
                }
                fcpu::DomainControllerRequest::LeaseMinFrequency {
                    domain_id,
                    frequency_index,
                    lease_name: _,
                    responder,
                } => {
                    let mut states = domain_states.borrow_mut();
                    let Some(state) = states.get_mut(&domain_id) else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                        continue;
                    };

                    if let Some(token) = state.lease_min(frequency_index) {
                        responder.send(Ok(token)).unwrap();
                    } else {
                        responder.send(Err(fcpu::DomainControllerError::InvalidArguments)).unwrap();
                    }
                }
                _ => unreachable!(),
            }
        }
    })
    .detach();
}

#[fuchsia::main]
async fn main() {
    info!("Started fake-domain-controller");

    // Create domains such that each field differs to allow a variety of test
    // cases for clients.
    let domains = Rc::new(vec![
        fcpu::DomainInfo {
            id: Some(0),
            core_ids: Some(vec![0, 1]),
            available_frequencies_hz: Some(vec![2024000000, 1512000000, 1256000000, 1128000000]),
            name: Some("test-cluster0".to_string()),
            ..Default::default()
        },
        fcpu::DomainInfo {
            id: Some(1),
            core_ids: Some(vec![2, 3, 4, 5]),
            available_frequencies_hz: Some(vec![1024000000, 512000000]),
            name: Some("test-cluster1".to_string()),
            ..Default::default()
        },
    ]);

    let domain_states: HashMap<u64, DomainState> = domains
        .iter()
        .filter_map(|d| {
            let freqs = d.available_frequencies_hz.as_ref()?.clone();
            d.id.map(|id| (id, DomainState::new(freqs)))
        })
        .collect();
    assert_eq!(domains.len(), domain_states.len());
    let domain_states = Rc::new(RefCell::new(domain_states));

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(move |stream| {
        handle_domain_controller_stream(stream, domains.clone(), domain_states.clone())
    });

    fs.take_and_serve_directory_handle().unwrap();
    fs.collect::<()>().await;
}
