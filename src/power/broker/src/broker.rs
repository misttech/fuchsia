// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::inspect::{
    AddElementInspectWriter, EagerInspectWriter, InspectAddDependency, InspectUpdateLevel,
    UpdateLevelInspectWriter,
};
use anyhow::{Context, Error, anyhow};
use async_utils::hanging_get::server::{HangingGet, Publisher, Subscriber};
use fidl_fuchsia_power_broker::{
    self as fpb, LeaseStatus, Permissions, RegisterDependencyTokenError, StatusError,
    StatusWatchPowerLevelResponder, UnregisterDependencyTokenError,
};
use fuchsia_inspect::{InspectType as IType, Node as INode};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug};
use std::hash::Hash;

use crate::credentials::*;
use crate::fpb::PowerLevel;
use crate::topology::*;

/// Max value for inspect event history.
const INSPECT_GRAPH_EVENT_BUFFER_SIZE: usize = 16384;

// Below are a series of type aliases for convenience
type LevelHangingGet<T> =
    HangingGet<IndexedPowerLevel, T, Box<dyn Fn(&IndexedPowerLevel, T) -> bool>>;
type LevelSubscriber<T> =
    Subscriber<IndexedPowerLevel, T, Box<dyn Fn(&IndexedPowerLevel, T) -> bool>>;
pub type CurrentLevelSubscriber = LevelSubscriber<StatusWatchPowerLevelResponder>;

trait Responder<T> {
    type Error;
    fn send(responder: T, result: Result<u8, Self::Error>) -> Result<(), fidl::Error>;
}

impl Responder<StatusWatchPowerLevelResponder> for StatusWatchPowerLevelResponder {
    type Error = StatusError;
    fn send(
        responder: StatusWatchPowerLevelResponder,
        result: Result<u8, StatusError>,
    ) -> Result<(), fidl::Error> {
        responder.send(result)
    }
}

struct LevelAdmin<T> {
    /// We pass new power level values to the publisher, which takes care of updating the remote
    /// clients using hanging-gets.
    publisher: Publisher<IndexedPowerLevel, T, Box<dyn Fn(&IndexedPowerLevel, T) -> bool>>,
    /// We use this to vend a new subscriber for each new watch request stream.
    hanging_get: HangingGet<IndexedPowerLevel, T, Box<dyn Fn(&IndexedPowerLevel, T) -> bool>>,
    /// Cached `IndexedPowerLevel` value. Simply used to determine if the value has changed.
    level: IndexedPowerLevel,
}

impl<T: Responder<T>> LevelAdmin<T> {
    fn new(initial_level: IndexedPowerLevel) -> Self {
        let hanging_get: HangingGet<
            IndexedPowerLevel,
            T,
            Box<dyn Fn(&IndexedPowerLevel, T) -> bool>,
        > = LevelHangingGet::<T>::new(
            initial_level,
            Box::new(|level: &IndexedPowerLevel, res: T| -> bool {
                if let Err(error) = T::send(res, Ok(level.level)).context("response failed") {
                    log::warn!(error:?; "Failed to send power level to client");
                }
                true
            }),
        );
        let publisher = hanging_get.new_publisher();
        LevelAdmin::<T> { publisher, hanging_get, level: initial_level }
    }
}

pub struct Broker {
    catalog: Catalog,
    credentials: Registry,
    // The current level for each element, as reported to the broker.
    current: HashMap<ElementID, LevelAdmin<StatusWatchPowerLevelResponder>>,
    // The level each element is transitioning to, if it is transitioning.
    in_transition: HashMap<ElementID, IndexedPowerLevel>,
    // The level for each element required by the topology.
    required: SubscribeMap<ElementID, IndexedPowerLevel>,
    lease_counter: HashMap<ElementID, HashMap<PowerLevel, i64>>,
    _inspect_node: INode,
}

impl Broker {
    pub fn new(inspect: INode) -> Self {
        Broker {
            catalog: Catalog::new(&inspect),
            credentials: Registry::new(),
            current: HashMap::new(),
            in_transition: HashMap::new(),
            required: SubscribeMap::new(None),
            lease_counter: HashMap::new(),
            _inspect_node: inspect,
        }
    }

    fn lookup_name(&self, element_id: ElementID) -> Cow<'_, str> {
        self.catalog.topology.element_name(element_id)
    }

    fn lookup_credentials(&self, token: &Token) -> Option<Credential> {
        self.credentials.lookup(token)
    }

    fn unregister_all_credentials_for_element(&mut self, element_id: ElementID) {
        self.credentials.unregister_all_for_element(element_id)
    }

    #[cfg(test)]
    fn get_unsatisfiable_element_id(&self) -> ElementID {
        self.catalog.topology.get_unsatisfiable_element_id()
    }

    #[cfg(test)]
    fn get_unsatisfiable_element_name(&self) -> String {
        self.catalog.topology.get_unsatisfiable_element_name()
    }

    #[cfg(test)]
    fn get_unsatisfiable_element_levels(&self) -> Vec<u64> {
        self.catalog.topology.get_unsatisfiable_element_levels()
    }

    pub fn register_dependency_token(
        &mut self,
        element_id: ElementID,
        token: Token,
    ) -> Result<(), RegisterDependencyTokenError> {
        let permissions = Permissions::MODIFY_DEPENDENT;
        match self
            .credentials
            .register(element_id, CredentialToRegister { broker_token: token, permissions })
        {
            Err(RegisterCredentialsError::AlreadyInUse) => {
                Err(RegisterDependencyTokenError::AlreadyInUse)
            }
            Err(RegisterCredentialsError::Internal) => Err(RegisterDependencyTokenError::Internal),
            Ok(_) => Ok(()),
        }
    }

    pub fn unregister_dependency_token(
        &mut self,
        element_id: ElementID,
        token: Token,
    ) -> Result<(), UnregisterDependencyTokenError> {
        let Some(credential) = self.lookup_credentials(&token) else {
            log::debug!("unregister_dependency_token: no element found matching requires_token");
            return Err(UnregisterDependencyTokenError::NotFound);
        };
        if credential.get_element() != element_id {
            log::debug!(
                "unregister_dependency_token: token is registered to {:?}, not {:?}",
                credential.get_element(),
                element_id,
            );
            return Err(UnregisterDependencyTokenError::NotAuthorized);
        }
        self.credentials.unregister(&credential);
        Ok(())
    }

    fn current_level_satisfies(&self, required: &ElementLevel) -> bool {
        self.get_current_level(&required.element_id)
            // If current level is unknown, required is not satisfied.
            .is_some_and(|current| {
                // When the element is transitioning between states, we can't assume that the
                // current level is still a valid comparison point. Only consider the level
                // satisfied if both the current and transition levels satisfy the required level.
                let current_level_satisfies = current.satisfies(required.level);
                if let Some(transit_level) = self.in_transition.get(&required.element_id) {
                    let transit_level_satisfies = transit_level.satisfies(required.level);
                    current_level_satisfies && transit_level_satisfies
                } else {
                    current_level_satisfies
                }
            })
    }

    // Deactivate claims that are now broken due to a disorderly level transition.
    // TODO(b/356400605): Consider simplifying this function with reverse dependency traversal
    // (Topology::dependencies_for_required_element), so that we do not have to scan all leases.
    // Note that narrowing the scan to leases claiming the disorderly element is not sufficient on
    // its own: a lease that is powering down may have already dropped that claim while still
    // enforcing claims that this function must break.
    fn deactivate_broken_claims(&mut self, element_id: ElementID, prev_level: IndexedPowerLevel) {
        let element_level = ElementLevel { element_id: element_id, level: prev_level.clone() };
        // For each lease, find the dependencies that are parents of the broken element.
        let mut affected_elements = HashSet::new();
        let mut affected_leases = HashSet::new();
        let dependencies_safe: HashSet<Dependency> = self
            .catalog
            .topology
            .all_direct_and_indirect_dependencies(&element_level)
            .into_iter()
            .collect();
        let leases: Vec<(LeaseID, Vec<ElementLevel>)> = self
            .catalog
            .leases
            .iter()
            .map(|(lease_id, lease)| (*lease_id, lease.targets.clone()))
            .collect();
        for (lease_id, targets) in leases {
            log::debug!("deactivate_broken_claims({lease_id}, {element_id}@{prev_level})");
            let mut dependencies_lease = HashSet::new();
            for target in &targets {
                dependencies_lease.insert(Dependency::new_root(target.clone()));
                dependencies_lease
                    .extend(self.catalog.topology.all_direct_and_indirect_dependencies(target));
            }
            if !dependencies_lease.iter().any(|d| {
                d.requires.element_id == element_id && d.requires.level.satisfies(prev_level)
            }) {
                log::debug!(
                    "There was no dependency in this lease that required this element to be at that level."
                );
                continue;
            }
            // Find the set difference between the dependencies of this lease and
            // the dependencies of the disorderly element. The element's dependencies
            // are 'safe' as none of their dependencies are broken. The difference
            // constitutes the dependencies of this lease that broke.
            let broken_dependencies = dependencies_lease
                .difference(&dependencies_safe)
                .cloned()
                .collect::<HashSet<Dependency>>();

            for dependency in broken_dependencies {
                if self.catalog.is_claim_enforced(&dependency, lease_id) {
                    affected_elements.insert(dependency.requires.element_id);
                    affected_leases.insert(lease_id);
                    if dependency.requires.element_id != element_id {
                        if self.catalog.is_lease_dropped(lease_id) {
                            self.catalog.drop_claim(&dependency, lease_id);
                        } else {
                            self.catalog.deactivate_claim(&dependency, lease_id);
                        }
                    }
                }
            }
        }
        affected_elements.remove(&element_id);
        let should_update_this_element = affected_elements.len() > 1;
        self.update_required_levels(affected_elements.into_iter(), &mut EagerInspectWriter);
        if should_update_this_element {
            self.update_required_level(element_id, prev_level.clone(), &mut EagerInspectWriter);
        }
        for lease in affected_leases {
            self.update_lease_status(lease);
            self.vacate_lease_if_all_claims_dropped(lease);
        }
    }

    pub fn update_current_level(&mut self, element_id: ElementID, level: IndexedPowerLevel) {
        fuchsia_trace::duration!("power-broker", "Broker::update_current_level");
        log::debug!("update_current_level({element_id}, {level:?})");
        let is_disorderly_update = self
            .required
            .get(&element_id)
            .is_some_and(|prev_required_level| !level.satisfies(prev_required_level));

        let mut inspect_writer = UpdateLevelInspectWriter::default();
        let prev_level = self.update_current_level_internal(element_id, level, &mut inspect_writer);
        inspect_writer.commit(&self.catalog.topology);

        // Check any deactivating claims on this element to see if they can now be dropped.
        // We do this even if the level has not changed, as the element may have been
        // transitioning the last time they were checked.
        let deactivating_claims: Vec<(Dependency, LeaseID)> =
            self.catalog.deactivating_for_dependent_element(element_id);
        if !deactivating_claims.is_empty() {
            let claims_with_no_dependents =
                self.find_deactivating_claims_to_drop(&deactivating_claims);
            self.drop_deactivating_claims(&claims_with_no_dependents);
        }

        if prev_level.as_ref() == Some(&level) {
            log::debug!("update_current_level({element_id}): level unchanged from {prev_level:?}");
            return;
        }
        if prev_level.is_none() || prev_level.unwrap() < level {
            // The level was increased, look for enforced claims that are newly
            // satisfied by the new current level:
            log::debug!(
                "update_current_level({element_id}): level increased from {prev_level:?} to {level:?}"
            );
            // Find dependencies that are newly satisfied by the new level of this element:
            let mut newly_satisfied_deps: Vec<Dependency> = Vec::new();
            for dep in self.catalog.all_dependencies_for_required_element(element_id) {
                if self.catalog.is_dependency_enforced(dep)
                    && level.satisfies(dep.requires.level)
                    && !prev_level.satisfies(dep.requires.level)
                {
                    newly_satisfied_deps.push(dep.clone());
                }
            }
            // Find the set of dependents for all claims satisfied:
            let dependents_of_claims_satisfied: HashSet<ElementID> = newly_satisfied_deps
                .iter()
                .filter_map(|d| d.dependent.as_ref().map(|dep| dep.element_id))
                .collect();
            // Because at least one of the dependencies of the dependent was
            // satisfied, other previously pending claims requiring the
            // dependent may now be ready to be activated (though they may not
            // if the dependent has other unsatisfied dependencies). Look for
            // all pending claims on this dependent, and pass them to
            // activate_claims_if_dependencies_satisfied(), which will check
            // if all dependencies of the dependent are now satisfied, and if
            // so, activate the pending claims on dependent, raising its
            // required level:
            for dependent in dependents_of_claims_satisfied.into_iter().sorted() {
                let pending_claims_on_dependent: Vec<(Dependency, LeaseID)> =
                    self.catalog.pending_for_required_element(dependent);
                self.activate_claims_if_dependencies_satisfied(pending_claims_on_dependent);
            }
            // Find the set of leases for all claims satisfied:
            let mut leases_to_check_if_satisfied: HashSet<LeaseID> = HashSet::new();
            for dep in &newly_satisfied_deps {
                if let Some(claims) = self.catalog.claims.get(dep) {
                    leases_to_check_if_satisfied.extend(claims.active.iter());
                }
            }
            // Update the status of all leases whose claims were satisfied.
            log::debug!(
                "update_current_level({element_id}): leases_to_check_if_satisfied = {:?}",
                leases_to_check_if_satisfied
            );
            for lease_id in leases_to_check_if_satisfied {
                self.update_lease_status(lease_id);
            }
            return;
        }
        if prev_level.unwrap() > level {
            log::debug!(
                "update_current_level({element_id}): level decreased from {prev_level:?} to {level:?}"
            );

            // Handle claims that were dropped unexpectedly, i.e.
            // 'disorderly' elements. When an element's level decreases
            // without a prior required level drop, we compute the set of
            // enforced claims that are no longer valid and immediately
            // deactivate them.
            if is_disorderly_update {
                self.deactivate_broken_claims(element_id, prev_level.unwrap().clone());
            }
        }
    }

    pub fn get_current_level(&self, element_id: &ElementID) -> Option<IndexedPowerLevel> {
        self.current.get(element_id).map(|e| e.level)
    }

    pub fn new_current_level_subscriber(
        &mut self,
        element_id: ElementID,
    ) -> CurrentLevelSubscriber {
        self.current
            .get_mut(&element_id)
            .ok_or_else(|| anyhow!("Element ({element_id}) not added"))
            .unwrap()
            .hanging_get
            .new_subscriber()
    }

    fn update_current_level_internal<I>(
        &mut self,
        element_id: ElementID,
        level: IndexedPowerLevel,
        inspect_writer: &mut I,
    ) -> Option<IndexedPowerLevel>
    where
        I: InspectUpdateLevel,
    {
        let previous = self.get_current_level(&element_id);
        if previous == Some(level) {
            return previous;
        }
        if let Some(current_level) = self.current.get_mut(&element_id) {
            current_level.publisher.set(level);
            current_level.level = level;
        } else {
            let level_admin = LevelAdmin::<StatusWatchPowerLevelResponder>::new(level);
            self.current.insert(element_id, level_admin);
        }

        inspect_writer.update_current_level(&self.catalog.topology, element_id, level.level);

        fuchsia_trace::counter!(
            c"power-broker", c"current_level", 0,
            &self.lookup_name(element_id).into_owned() => level.level as u32
        );

        if let Some(transit_level) = self.in_transition.get(&element_id) {
            if *transit_level == level {
                log::debug!(
                    "update_current_level_internal({element_id}): transitioned to {level:?}"
                );
                self.in_transition.remove(&element_id);
                self.update_required_levels([element_id].into_iter(), inspect_writer);
            }
        }
        previous
    }

    pub fn get_required_level(&self, element_id: &ElementID) -> Option<IndexedPowerLevel> {
        self.required.get(element_id)
    }

    pub fn watch_required_level(
        &mut self,
        element_id: &ElementID,
    ) -> UnboundedReceiver<Option<IndexedPowerLevel>> {
        self.required.subscribe(element_id)
    }

    fn update_required_level<I>(
        &mut self,
        element_id: ElementID,
        level: IndexedPowerLevel,
        inspect_writer: &mut I,
    ) -> Option<IndexedPowerLevel>
    where
        I: InspectUpdateLevel,
    {
        let previous = self.get_required_level(&element_id);
        if previous == Some(level) {
            return previous;
        }
        if self.in_transition.contains_key(&element_id) {
            return previous;
        }
        self.required.update(&element_id, level);
        if let Some(current_level) = self.current.get(&element_id) {
            if current_level.level != level {
                log::debug!("update_required_level({element_id}): transitioning to {level}");
                self.in_transition.insert(element_id, level);
            }
        }
        inspect_writer.update_required_level(&self.catalog.topology, element_id, level.level);
        fuchsia_trace::counter!(
            c"power-broker", c"required_level", 0,
            &self.lookup_name(element_id).into_owned() => level.level as u32
        );

        previous
    }

    fn adjust_lease_counter(&mut self, element_id: ElementID, level: PowerLevel, adj: i64) -> i64 {
        if let Some(inner_map) = self.lease_counter.get_mut(&element_id) {
            if let Some(counter) = inner_map.get_mut(&level) {
                *counter += adj;
                return *counter;
            } else {
                inner_map.insert(level, adj);
                return adj;
            }
        } else {
            let mut inner_map = HashMap::new();
            inner_map.insert(level, adj);
            self.lease_counter.insert(element_id, inner_map);
            return adj;
        }
    }

    pub fn acquire_lease(
        &mut self,
        element_id: ElementID,
        level: IndexedPowerLevel,
        lease_control: zx::Koid,
    ) -> Result<Lease, fpb::LeaseError> {
        fuchsia_trace::duration!("power-broker", "Broker::acquire_lease");
        log::debug!("acquire_lease({element_id}@{level})");
        let counter = self.adjust_lease_counter(element_id, level.level, 1) as i64;
        fuchsia_trace::counter!(
            c"power-broker", c"LeaseCounter", level.level.into(),
            &self.lookup_name(element_id).into_owned() => counter
        );

        let targets = vec![ElementLevel { element_id, level }];
        let (lease, deps_claimed) =
            self.catalog.create_lease_and_claim_dependencies(None, targets, lease_control);
        // Activate all pending claims that have all of their
        // dependencies satisfied.
        self.activate_claims_if_dependencies_satisfied(
            deps_claimed.into_iter().map(|dep| (dep, lease.id)),
        );
        self.update_lease_status(lease.id);
        Ok(lease)
    }

    pub fn acquire_direct_lease(
        &mut self,
        lease_name: String,
        dependencies: Vec<fpb::LeaseDependency>,
        lease_control: zx::Koid,
    ) -> Result<Lease, fpb::LeaseError> {
        fuchsia_trace::duration!("power-broker", "Broker::acquire_direct_lease");
        log::debug!("acquire_direct_lease({lease_name})");
        // A lease with no dependencies would have no targets and no claims. It
        // would be immediately Satisfied and invisible in Inspect, so reject it.
        if dependencies.is_empty() {
            log::warn!("acquire_direct_lease: dependencies must not be empty");
            return Err(fpb::LeaseError::InvalidArgument);
        }
        let mut required_levels = Vec::new();
        for dependency in dependencies {
            let Some(requires_token) = dependency.requires_token else {
                log::error!("acquire_direct_lease: dependency is missing requires_token");
                return Err(fpb::LeaseError::NotAuthorized);
            };
            let requires_token = Token::from(requires_token);
            let Some(requires_cred) = self.credentials.lookup(&requires_token) else {
                log::error!("acquire_direct_lease: unable to find required credentials");
                return Err(fpb::LeaseError::NotAuthorized);
            };
            let requires_element_id = requires_cred.get_element();
            let requires_level = if let Some(levels) = dependency.requires_level_by_preference {
                levels
                    .iter()
                    .find_map(|l| self.catalog.topology.get_level_index(requires_element_id, l))
                    .ok_or(fpb::LeaseError::InvalidLevel)?
            } else if let Some(level) = dependency.requires_level {
                self.catalog
                    .topology
                    .get_level_index(requires_element_id, &level)
                    .ok_or(fpb::LeaseError::InvalidLevel)?
            } else {
                return Err(fpb::LeaseError::InvalidLevel);
            };
            required_levels
                .push(ElementLevel { element_id: requires_element_id, level: *requires_level });
        }

        let (lease, deps_claimed) = self.catalog.create_lease_and_claim_dependencies(
            Some(&lease_name),
            required_levels,
            lease_control,
        );

        // Count the lease's targets, which `Lease::new` has deduplicated. `drop_lease` decrements
        // the same set, so any duplicates must be collapsed before we count them.
        for target in &lease.targets {
            let counter = self.adjust_lease_counter(target.element_id, target.level.level, 1);
            fuchsia_trace::counter!(
                c"power-broker", c"LeaseCounter", target.level.level.into(),
                &self.lookup_name(target.element_id).into_owned() => counter
            );
        }

        // Activate all pending claims that have all of their
        // dependencies satisfied.
        self.activate_claims_if_dependencies_satisfied(
            deps_claimed.into_iter().map(|dep| (dep, lease.id)),
        );
        self.update_lease_status(lease.id);
        Ok(lease)
    }

    pub fn drop_lease(&mut self, lease_id: LeaseID) -> Result<(), Error> {
        fuchsia_trace::duration!("power-broker", "Broker::drop_lease");
        // Drop pending claims, mark active claims to deactivate, and transition the lease to
        // PoweringDown.
        let lease = self.catalog.drop_and_mark_powering_down(lease_id)?;
        for target in &lease.targets {
            let counter =
                self.adjust_lease_counter(target.element_id, target.level.level, -1) as i64;
            fuchsia_trace::counter!(
                c"power-broker", c"LeaseCounter", target.level.level.into(),
                &self.lookup_name(target.element_id).into_owned() => counter
            );
        }

        // Find any remaining deactivating claims that can be safely dropped immediately.
        let deactivating_claims = self.catalog.deactivating_for_lease(lease_id);
        if !deactivating_claims.is_empty() {
            let claims_to_drop = self.find_deactivating_claims_to_drop(&deactivating_claims);
            // Drop the discovered set of claims and update required levels.
            self.drop_deactivating_claims(&claims_to_drop);
        }

        // Check if the lease has no remaining claims and can be vacated immediately.
        self.vacate_lease_if_all_claims_dropped(lease_id);
        Ok(())
    }

    fn vacate_lease_if_all_claims_dropped(&mut self, lease_id: LeaseID) {
        self.vacate_lease_with_removed_element(lease_id, None);
    }

    fn vacate_lease_with_removed_element(
        &mut self,
        lease_id: LeaseID,
        removed_element: Option<&Element>,
    ) {
        if self.catalog.is_lease_powering_down(lease_id)
            && self.catalog.has_no_remaining_claims(lease_id)
        {
            if let Err(err) = self.vacate_lease(lease_id, removed_element) {
                log::error!("Failed to vacate lease {lease_id}: {:?}", err);
            }
        }
    }

    fn vacate_lease(
        &mut self,
        lease_id: LeaseID,
        removed_element: Option<&Element>,
    ) -> Result<(), Error> {
        log::debug!("vacate_lease({lease_id})");
        let lease = self
            .catalog
            .leases
            .get(&lease_id)
            .cloned()
            .ok_or_else(|| anyhow!("{lease_id} not found"))?;

        self.catalog.lease_status.update(&lease_id, LeaseStatus::Vacated);
        for target in &lease.targets {
            if let Some(element) = self
                .catalog
                .topology
                .get_element(&target.element_id)
                .or_else(|| removed_element.filter(|e| e.id == target.element_id))
            {
                self.catalog.topology.inspect().on_update_lease_status(
                    element,
                    &lease,
                    &LeaseStatus::Vacated,
                );
                self.catalog.topology.inspect().on_drop_lease(element, &lease);
            }
        }

        self.catalog.leases.remove(&lease_id);
        self.catalog.lease_status.remove(&lease_id);
        Ok(())
    }

    fn calculate_lease_status(&self, lease_id: LeaseID) -> LeaseStatus {
        if let Some(deps) = self.catalog.lease_dependencies.get(&lease_id) {
            for dep in deps {
                if !self.catalog.is_claim_enforced(dep, lease_id)
                    || !self.current_level_satisfies(&dep.requires)
                {
                    return LeaseStatus::Pending;
                }
            }
        }
        // All claims are satisfied, so the lease is Satisfied.
        LeaseStatus::Satisfied
    }

    /// Re-evaluates the lease status and updates the status map.
    /// Returns the status if the overall status has changed.
    /// Returns None if no change occurred or if the lease has already been dropped.
    fn update_lease_status(&mut self, lease_id: LeaseID) -> Option<LeaseStatus> {
        // Return immediately if the lease has already dropped.
        if self.catalog.is_lease_dropped(lease_id) {
            return None;
        }
        // Calculate the current and previous status state.
        let status = self.calculate_lease_status(lease_id);
        let prev_status = self.catalog.lease_status.update(&lease_id, status);
        // Return if no state change has occurred.
        if prev_status.as_ref() == Some(&status) {
            return None;
        };
        log::debug!("update_lease_status({lease_id}) to {status:?}");
        let lease = match self.catalog.leases.get(&lease_id) {
            Some(lease) => lease.clone(),
            None => unreachable!("The lease must be present when updating the status."),
        };
        for target in &lease.targets {
            if let Some(element) = self.catalog.topology.get_element(&target.element_id) {
                self.catalog.topology.inspect().on_update_lease_status(&element, &lease, &status);
            }
        }
        Some(status)
    }

    #[cfg(test)]
    pub fn get_lease_status(&self, lease_id: LeaseID) -> Option<LeaseStatus> {
        self.catalog.get_lease_status(&lease_id)
    }

    pub fn watch_lease_status(
        &mut self,
        lease_id: LeaseID,
    ) -> UnboundedReceiver<Option<LeaseStatus>> {
        self.catalog.watch_lease_status(&lease_id)
    }

    fn update_required_levels<I>(
        &mut self,
        element_ids: impl IntoIterator<Item = ElementID>,
        inspect_writer: &mut I,
    ) where
        I: InspectUpdateLevel,
    {
        for element_id in element_ids {
            let new_required_level = self.catalog.calculate_required_level(element_id);
            log::debug!("update required level({:?}, {:?})", element_id, new_required_level);
            self.update_required_level(element_id, new_required_level, inspect_writer);
        }
    }

    /// Examines a Vec of pending claims and activates each for which
    /// either the required element is already at the required level (and thus
    /// the claim is already satisfied) or all of the dependencies of its required
    /// ElementLevel are met. Updates required levels of affected elements.
    /// For example, let us imagine elements A, B, C and D where A depends on B
    /// and B depends on C and D. In order to activate the A->B claim, all
    /// dependencies of B (i.e. B->C and B->D) must first be satisfied.
    fn activate_claims_if_dependencies_satisfied(
        &mut self,
        pending_claims: impl IntoIterator<Item = (Dependency, LeaseID)>,
    ) {
        let claims_to_activate: Vec<(Dependency, LeaseID)> = pending_claims
            .into_iter()
            .filter(|(dep, _lease_id)| {
                self.current_level_satisfies(&dep.requires)
                    || self.all_dependencies_satisfied(&dep.requires)
            })
            .collect();

        let mut element_ids = Vec::new();
        for (dep, lease_id) in claims_to_activate {
            element_ids.push(dep.requires.element_id);
            self.catalog.activate_claim(&dep, lease_id);
        }

        self.update_required_levels(element_ids.into_iter().unique(), &mut EagerInspectWriter);
    }

    /// Examines the direct dependencies of an element level
    /// and returns true if they are all satisfied (current level >= required).
    fn all_dependencies_satisfied(&self, element_level: &ElementLevel) -> bool {
        self.catalog.topology.all_direct_and_indirect_dependencies(&element_level).into_iter().all(
            |dep| {
                if !self.current_level_satisfies(&dep.requires) {
                    log::debug!(
                        "dependency {dep:?} of element_level {element_level:?} is not satisfied: \
                    current level of {:?} = {:?}, {:?} required",
                        dep.requires.element_id,
                        self.get_current_level(&dep.requires.element_id),
                        dep.requires.level
                    );
                    return false;
                }
                true
            },
        )
    }

    /// Examines a slice of deactivating claims and returns any that can be dropped immediately
    /// (either because another open lease has an active claim satisfying the required level, or
    /// because the dependent element is no longer satisfied and no other enforced claim within the
    /// same lease requires it).
    fn find_deactivating_claims_to_drop(
        &self,
        claims: &[(Dependency, LeaseID)],
    ) -> Vec<(Dependency, LeaseID)> {
        log::debug!(
            "find_deactivating_claims_to_drop: [{}]",
            claims.iter().map(|(d, l)| format!("{l}:{d}")).join("; ")
        );
        let mut claims_to_drop = Vec::new();

        for (dep, lease_id) in claims {
            // Callers (`deactivating_for_dependent_element` and `deactivating_for_lease`) only
            // collect claims in `deactivating`, so any non-deactivating claim here violates the
            // caller contract.
            if !self.catalog.is_claim_deactivating(dep, *lease_id) {
                debug_assert!(
                    false,
                    "claim {lease_id}:{dep} passed to find_deactivating_claims_to_drop is not deactivating"
                );
                continue;
            }
            // If the dependent element is transiting, we cannot drop this claim as
            // we cannot guarantee that it hasn't yet dropped to its destination level.
            if let Some(dependent) = &dep.dependent {
                if self.in_transition.contains_key(&dependent.element_id) {
                    log::debug!("keeping {lease_id}:{dep}, dependent is transiting");
                    continue;
                }
            }
            // If another open lease has an active claim whose required level satisfies its
            // required level, we can drop this claim immediately.
            let has_other_active_claim =
                self.catalog.claims.get(dep).is_some_and(|c| c.has_other_active(lease_id))
                    || self
                        .catalog
                        .all_dependencies_for_required_element(dep.requires.element_id)
                        .any(|other_dep| {
                            let dep_satisfied = match (&other_dep.dependent, &dep.dependent) {
                                (Some(d2), Some(d1)) => d2.satisfies(d1),
                                (None, None) => true,
                                _ => false,
                            };
                            other_dep != dep
                                && dep_satisfied
                                && other_dep.requires.satisfies(&dep.requires)
                                && self
                                    .catalog
                                    .claims
                                    .get(other_dep)
                                    .is_some_and(|c| c.has_other_active(lease_id))
                        });

            if has_other_active_claim {
                log::debug!(
                    "required level still required by another lease's active claim, will drop {lease_id}:{dep}"
                );
                claims_to_drop.push((dep.clone(), *lease_id));
                continue;
            }
            if let Some(dependent) = &dep.dependent {
                if self.current_level_satisfies(dependent) {
                    log::debug!("keeping {lease_id}:{dep}, dependent is still satisfied");
                    continue;
                }
            }
            let mut has_dependents = false;
            if let Some(dep_dependent) = &dep.dependent {
                // Only claims belonging to the same lease can be a dependent.
                for related_dep in self.catalog.enforced_for_lease(*lease_id) {
                    if dep_dependent.element_id == related_dep.requires.element_id
                        && dep_dependent.level >= related_dep.requires.level
                        && self.current_level_satisfies(&related_dep.requires)
                    {
                        log::debug!("won't drop {lease_id}:{dep}, has dependent {related_dep}");
                        has_dependents = true;
                        break;
                    }
                }
            }
            if has_dependents {
                continue;
            }
            log::debug!("will drop {lease_id}:{dep}");
            claims_to_drop.push((dep.clone(), *lease_id));
        }
        claims_to_drop
    }

    /// Drops a slice of deactivating claims, updates required levels of affected elements, and
    /// vacates any leases whose remaining claims have all been dropped.
    fn drop_deactivating_claims(&mut self, claims: &[(Dependency, LeaseID)]) {
        let mut leases_to_check = HashSet::new();
        for (dep, lease_id) in claims {
            log::debug!("drop deactivating claim: {lease_id}:{dep}");
            // Claims only enter `deactivating` when `drop_and_mark_powering_down` transitions
            // their lease to `PoweringDown`, so every deactivating claim must belong to a dropped
            // lease.
            debug_assert!(
                self.catalog.is_lease_dropped(*lease_id),
                "deactivating claim {lease_id}:{dep} must belong to a dropped lease"
            );
            self.catalog.drop_claim(dep, *lease_id);
            leases_to_check.insert(*lease_id);
        }
        self.update_required_levels(
            claims.iter().map(|(dep, _)| dep.requires.element_id).unique(),
            &mut EagerInspectWriter,
        );
        for lease_id in leases_to_check {
            self.vacate_lease_if_all_claims_dropped(lease_id);
        }
    }

    pub fn add_element(
        &mut self,
        name: &str,
        initial_current_level: fpb::PowerLevel,
        valid_levels: Vec<fpb::PowerLevel>,
        level_dependencies: Vec<fpb::LevelDependency>,
    ) -> Result<ElementID, AddElementError> {
        fuchsia_trace::duration!("power-broker", "Broker::add_element");
        if valid_levels.len() < 1 {
            return Err(AddElementError::Invalid);
        }
        let id = self.catalog.topology.add_element(name, &valid_levels)?;
        let initial_current_level = *self
            .catalog
            .topology
            .get_level_index(id, &initial_current_level)
            .ok_or(AddElementError::Invalid)?;

        let mut inspect_writer = AddElementInspectWriter::new(id);
        self.update_current_level_internal(id, initial_current_level, &mut inspect_writer);
        let minimum_level = self.catalog.topology.minimum_level(id);
        self.update_required_level(id, minimum_level, &mut inspect_writer);

        for dependency in level_dependencies {
            if let Err(err) = self.add_dependency(id, dependency, &mut inspect_writer) {
                // Clean up by removing the element we just added.
                inspect_writer.commit(&mut self.catalog.topology);
                self.remove_element(&id);
                return Err(match err {
                    ModifyDependencyError::AlreadyExists => AddElementError::Invalid,
                    ModifyDependencyError::Invalid => AddElementError::Invalid,
                    ModifyDependencyError::NotFound(_) => AddElementError::Invalid,
                    ModifyDependencyError::NotAuthorized => AddElementError::NotAuthorized,
                });
            }
        }
        inspect_writer.commit(&mut self.catalog.topology);
        Ok(id)
    }

    #[cfg(test)]
    fn element_exists(&self, element_id: ElementID) -> bool {
        self.catalog.topology.element_exists(element_id)
    }

    pub fn remove_element(&mut self, element_id: &ElementID) {
        fuchsia_trace::duration!("power-broker", "Broker::remove_element");
        log::debug!("removing element {element_id}");
        // Before removing the element, clear any transiting state and simulate the
        // downward transition from its transiting level to its minimum level. This
        // ensures that all associated claims are cleared.
        let minimum_level = self.catalog.minimum_level(*element_id);
        if let Some(transition_level) = self.in_transition.get(&element_id) {
            if let Some(current_level) = self.current.get_mut(&element_id) {
                current_level.level = max(*transition_level, current_level.level);
            }
        }
        self.in_transition.remove(&element_id);

        // Drop any open leases targeting this element while its vertex is still in the topology so
        // their Inspect lifecycle events are emitted before the element vertex is removed.
        let leases_targeting_element: Vec<LeaseID> = self
            .catalog
            .leases
            .iter()
            .filter(|(lease_id, lease)| {
                !self.catalog.is_lease_dropped(**lease_id)
                    && lease.targets.iter().any(|target| target.element_id == *element_id)
            })
            .map(|(lease_id, _)| *lease_id)
            .sorted()
            .collect();
        for lease_id in leases_targeting_element {
            if let Err(err) = self.drop_lease(lease_id) {
                log::warn!("Failed to drop lease {lease_id} during element removal: {:?}", err);
            }
        }

        // Remove all removable dependencies that require this element.
        let removable_deps =
            self.catalog.topology.get_removable_dependencies_for_required_element(*element_id);
        for dep in removable_deps {
            self.remove_dependency_and_update_leases(&dep);
        }

        let affected_leases = self.catalog.find_leases_affected_by_removal(*element_id);

        self.update_current_level(*element_id, minimum_level);
        // Remove all references of the element from the topology.
        let maybe_element = self.catalog.topology.remove_element(*element_id);

        // Cleanup orphaned claims now that the element and its dependencies are gone.
        let (elements_to_update, affected_leases) =
            self.catalog.cleanup_orphaned_claims_for_leases(affected_leases);
        self.update_required_levels(elements_to_update.into_iter(), &mut EagerInspectWriter);
        for lease_id in affected_leases {
            self.update_lease_status(lease_id);
            self.vacate_lease_with_removed_element(lease_id, maybe_element.as_ref());
        }

        self.unregister_all_credentials_for_element(*element_id);
        let current_level = self.current.remove(&element_id);
        let required_level = self.required.remove(&element_id);
        if let Some(element) = maybe_element {
            // If any lease targeting this element is still powering down (for example, on
            // upstream ancestors or other target elements), emit its `rm_lease` event before
            // removing the element vertex.
            for (_, lease) in self.catalog.leases.iter().sorted_by_key(|(id, _)| *id) {
                if lease.targets.iter().any(|target| target.element_id == element.id) {
                    self.catalog.topology.inspect().on_drop_lease(&element, lease);
                }
            }
            self.catalog.topology.inspect().on_remove_element(
                element,
                current_level.map(|level| level.level.level),
                required_level.map(|level| level.level),
            );
        }
        self.lease_counter.remove(&element_id);
    }

    fn remove_dependency_and_update_leases(&mut self, dep: &Dependency) {
        if let Ok(()) = self.catalog.topology.remove_dependency(dep) {
            let mut affected_leases = HashSet::new();
            if let Some(claims) = self.catalog.claims.get(dep) {
                affected_leases.extend(claims.all_leases());
            }
            for lease_id in &affected_leases {
                self.catalog.drop_claim(dep, *lease_id);
            }
            self.update_required_levels(
                [dep.requires.element_id].into_iter(),
                &mut EagerInspectWriter,
            );
            for lease_id in affected_leases {
                self.update_lease_status(lease_id);
                self.vacate_lease_if_all_claims_dropped(lease_id);
            }
        }
    }

    pub fn get_level_index(
        &self,
        element_id: ElementID,
        level: &fpb::PowerLevel,
    ) -> Option<&IndexedPowerLevel> {
        self.catalog.topology.get_level_index(element_id, level)
    }

    /// Checks authorization and looks up the required element ID and level for a dependency.
    fn lookup_dependency_requires(
        &self,
        dependency: &fpb::LevelDependency,
    ) -> Result<(ElementID, IndexedPowerLevel), ModifyDependencyError> {
        let requires_token =
            dependency.requires_token.as_ref().ok_or(ModifyDependencyError::Invalid)?;
        let Some(requires_cred) = self.lookup_credentials(&Token::from(requires_token)) else {
            return Err(ModifyDependencyError::NotAuthorized);
        };
        if !requires_cred.contains(Permissions::MODIFY_DEPENDENT) {
            return Err(ModifyDependencyError::NotAuthorized);
        }
        let requires_element_id = requires_cred.get_element();

        let requires_level = dependency
            .requires_level_by_preference
            .as_ref()
            .ok_or(ModifyDependencyError::Invalid)?
            .iter()
            .find_map(|l| self.catalog.topology.get_level_index(requires_element_id.clone(), l))
            .ok_or(ModifyDependencyError::Invalid)?
            .clone();

        Ok((requires_element_id, requires_level))
    }

    /// Prepares to add a dependency by acquiring a provisional lease if needed.
    /// Returns the Lease if a provisional lease was acquired.
    pub fn prepare_add_dependency(
        &mut self,
        element_id: ElementID,
        dependency: &fpb::LevelDependency,
    ) -> Result<Option<Lease>, ModifyDependencyError> {
        let (requires_element_id, requires_level) = self.lookup_dependency_requires(dependency)?;

        let current_required = self.get_required_level(&element_id);
        let dependent_level =
            dependency.dependent_level.as_ref().ok_or(ModifyDependencyError::Invalid)?;
        let dep_level_idx = self.get_level_index(element_id, dependent_level);

        let need_provisional_lease =
            dep_level_idx.map(|dep| current_required.satisfies(dep.clone())).unwrap_or(false);

        let provisional_lease = if need_provisional_lease {
            let lease_token = zx::Event::create();
            let lease_koid = lease_token.koid().unwrap();
            let lease = self
                .acquire_lease(requires_element_id.clone(), requires_level, lease_koid)
                .unwrap();
            Some(lease)
        } else {
            None
        };

        Ok(provisional_lease)
    }

    /// Checks authorization from requires_token, and if valid, adds a dependency to the Topology.
    pub fn add_dependency<I>(
        &mut self,
        element_id: ElementID,
        dependency: fpb::LevelDependency,
        inspect_writer: &mut I,
    ) -> Result<(), ModifyDependencyError>
    where
        I: InspectAddDependency,
    {
        let (requires_element_id, requires_level) = self.lookup_dependency_requires(&dependency)?;
        let dependent_level_val =
            dependency.dependent_level.as_ref().ok_or(ModifyDependencyError::Invalid)?;
        let dependent_level = self
            .catalog
            .topology
            .get_level_index(element_id, dependent_level_val)
            .ok_or(ModifyDependencyError::Invalid)?;
        let dep = Dependency {
            dependent: Some(ElementLevel { element_id: element_id, level: *dependent_level }),
            requires: ElementLevel {
                element_id: requires_element_id.clone(),
                level: requires_level,
            },
        };
        let on_required_element_removal =
            OnRequiredElementRemoval::from_level_dependency(dependency);
        self.catalog.topology.add_dependency(&dep, on_required_element_removal, inspect_writer)?;
        self.update_leases_for_dependency(dep);
        Ok(())
    }

    fn update_leases_for_dependency(&mut self, dependency: Dependency) {
        let Some(dependent) = &dependency.dependent else {
            return;
        };
        let dependent_level = dependent.level;
        let dependent_id = dependent.element_id;

        let mut leases_to_update: HashSet<LeaseID> = HashSet::new();
        for dep in self.catalog.all_dependencies_for_required_element(dependent_id) {
            if dep.requires.level >= dependent_level {
                if let Some(claims) = self.catalog.claims.get(dep) {
                    // `drop_and_mark_powering_down` immediately drops all `pending` claims and
                    // moves all `active` claims to `deactivating`, so `pending` and `active` only
                    // ever contain open (`!is_lease_dropped`) leases.
                    debug_assert!(
                        claims
                            .pending
                            .iter()
                            .chain(claims.active.iter())
                            .all(|lease_id| !self.catalog.is_lease_dropped(*lease_id)),
                        "pending and active sets for {dep} must only contain open leases"
                    );
                    leases_to_update
                        .extend(claims.pending.iter().chain(claims.active.iter()).copied());
                }
            }
        }

        let mut new_dependencies =
            self.catalog.topology.all_direct_and_indirect_dependencies(&dependency.requires);
        new_dependencies.push(dependency);

        // Claim dependencies in a deterministic order, so that required levels are updated in a
        // consistent order as well.
        let leases_to_update: Vec<LeaseID> = leases_to_update.into_iter().sorted().collect();
        let mut all_new_claims = Vec::new();
        for lease_id in &leases_to_update {
            let deps_to_claim: Vec<Dependency> = new_dependencies
                .iter()
                .filter(|dep| {
                    !self
                        .catalog
                        .lease_dependencies
                        .get(lease_id)
                        .is_some_and(|deps| deps.contains(*dep))
                })
                .cloned()
                .collect();

            let deps_claimed = self.catalog.filter_out_redundant_dependencies(deps_to_claim);
            for dep in deps_claimed {
                self.catalog.add_pending_claim(dep.clone(), *lease_id);
                all_new_claims.push((dep, *lease_id));
            }
        }

        self.activate_claims_if_dependencies_satisfied(all_new_claims);

        for lease_id in leases_to_update {
            self.update_lease_status(lease_id);
        }
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct LeaseID(u64);

impl fmt::Display for LeaseID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Deref for LeaseID {
    type Target = u64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialOrd, PartialEq)]
pub struct Lease {
    pub id: LeaseID,
    /// The element levels this lease directly demands. Each target is claimed
    /// via a root dependency (see [`Dependency::new_root`]).
    targets: Vec<ElementLevel>,
}

impl Lease {
    fn new(targets: Vec<ElementLevel>, lease_control: zx::Koid) -> Self {
        // A lease claims one root dependency per element, at the highest level it demands (see
        // Catalog::filter_out_redundant_dependencies), and Inspect keys a lease record by element.
        // Collapse targets the same way so that each one maps to exactly one claim and one record.
        let mut collapsed: Vec<ElementLevel> = Vec::new();
        for target in targets {
            match collapsed.iter_mut().find(|t| t.element_id == target.element_id) {
                Some(existing) => existing.level = existing.level.max(target.level),
                None => collapsed.push(target),
            }
        }
        Lease { id: LeaseID(lease_control.raw_koid()), targets: collapsed }
    }
}

/// Manages the power topology, leases, and claims (`pending` <-> `active` -> `deactivating` ->
/// dropped).
#[derive(Debug)]
struct Catalog {
    topology: Topology,
    leases: HashMap<LeaseID, Lease>,
    lease_status: SubscribeMap<LeaseID, LeaseStatus>,
    claims: FxHashMap<Dependency, Claims>,
    lease_dependencies: FxHashMap<LeaseID, FxHashSet<Dependency>>,
    /// Claimed root dependencies, indexed by required element. Root dependencies have no dependent
    /// element, so unlike all other dependencies they are not part of the topology graph and are
    /// not covered by `Topology`'s indexes.
    root_deps_by_required_element: FxHashMap<ElementID, FxHashSet<Dependency>>,
    pending_claims_count: usize,
    enforced_claims_count: usize,
}

impl Catalog {
    fn new(inspect_parent: &INode) -> Self {
        // Note: the `claims_pending` and `claims_activated` trace counter names are preserved from
        // when claims were tracked individually, so that existing traces remain comparable.
        fuchsia_trace::counter!(
            c"power-broker", c"claims_pending", 0,
            "claims" => 0 as u32
        );
        fuchsia_trace::counter!(
            c"power-broker", c"claims_activated", 0,
            "claims" => 0 as u32
        );
        Catalog {
            topology: Topology::new(inspect_parent, INSPECT_GRAPH_EVENT_BUFFER_SIZE),
            leases: HashMap::new(),
            lease_status: SubscribeMap::new(Some(inspect_parent.create_child("leases"))),
            claims: FxHashMap::default(),
            lease_dependencies: FxHashMap::default(),
            root_deps_by_required_element: FxHashMap::default(),
            pending_claims_count: 0,
            enforced_claims_count: 0,
        }
    }

    /// Returns an iterator over all claimed dependencies where `element_id` is the required
    /// element, including root dependencies.
    ///
    /// Prefer this over `Topology::dependencies_for_required_element`, which only covers the
    /// topology graph and therefore omits lease targets.
    fn all_dependencies_for_required_element(
        &self,
        element_id: ElementID,
    ) -> impl Iterator<Item = &Dependency> {
        self.topology.dependencies_for_required_element(element_id).chain(
            self.root_deps_by_required_element.get(&element_id).into_iter().flat_map(|d| d.iter()),
        )
    }

    fn find_leases_affected_by_removal(&self, removed_element_id: ElementID) -> HashSet<LeaseID> {
        let mut affected_leases = HashSet::new();

        // 1. Leases with claims requiring removed_element_id
        for dep in self.all_dependencies_for_required_element(removed_element_id) {
            if let Some(entry) = self.claims.get(dep) {
                affected_leases.extend(entry.all_leases());
            }
        }

        // 2. Leases with claims where removed_element_id is the dependent.
        for dep in self.topology.dependencies_for_dependent_element(removed_element_id) {
            if let Some(entry) = self.claims.get(dep) {
                affected_leases.extend(entry.all_leases());
            }
        }

        // 3. Leases that target removed_element_id (including leases whose direct claims on
        // removed_element_id were dropped while pending when the lease was dropped, leaving
        // active/deactivating claims several hops away).
        for (lease_id, lease) in &self.leases {
            if lease.targets.iter().any(|target| target.element_id == removed_element_id) {
                affected_leases.insert(*lease_id);
            }
        }
        affected_leases
    }

    /// Called after an element is removed, to remove any claims that are no longer valid from the
    /// given leases.
    fn cleanup_orphaned_claims_for_leases(
        &mut self,
        lease_ids: HashSet<LeaseID>,
    ) -> (HashSet<ElementID>, HashSet<LeaseID>) {
        let mut elements_to_update = HashSet::new();
        let mut affected_leases = HashSet::new();

        for lease_id in lease_ids {
            let Some(lease) = self.leases.get(&lease_id).cloned() else {
                continue;
            };
            // 1. Recalculate the valid dependencies for the lease.
            let mut valid_deps: HashSet<Dependency> = HashSet::new();
            for target in &lease.targets {
                if self.topology.get_element(&target.element_id).is_some() {
                    valid_deps.insert(Dependency::new_root(target.clone()));
                    valid_deps.extend(self.topology.all_direct_and_indirect_dependencies(target));
                }
            }

            // 2. Remove any orphaned claims for this lease. Removing an enforced claim
            // affects both the required level of the required element and the lease's status.
            let Some(lease_deps) = self.lease_dependencies.get(&lease_id).cloned() else {
                continue;
            };
            for dep in lease_deps {
                if !valid_deps.contains(&dep) {
                    if self.is_claim_enforced(&dep, lease_id) {
                        elements_to_update.insert(dep.requires.element_id);
                    }
                    self.drop_claim(&dep, lease_id);
                    affected_leases.insert(lease_id);
                }
            }
        }
        (elements_to_update, affected_leases)
    }

    fn minimum_level(&self, element_id: ElementID) -> IndexedPowerLevel {
        self.topology.minimum_level(element_id)
    }

    /// Returns true if the lease was dropped (or never existed).
    fn is_lease_dropped(&self, lease_id: LeaseID) -> bool {
        if !self.leases.contains_key(&lease_id) {
            return true;
        }
        match self.lease_status.get(&lease_id) {
            Some(LeaseStatus::PoweringDown) | Some(LeaseStatus::Vacated) => true,
            _ => false,
        }
    }

    fn is_lease_powering_down(&self, lease_id: LeaseID) -> bool {
        self.lease_status.get(&lease_id) == Some(LeaseStatus::PoweringDown)
    }

    fn has_no_remaining_claims(&self, lease_id: LeaseID) -> bool {
        self.lease_dependencies.get(&lease_id).is_none_or(|deps| deps.is_empty())
    }

    /// Calculates the required level for each element, according to the
    /// Minimum Power Level Policy.
    /// The required level is equal to the maximum of all **enforced**
    /// claims on the element, or the element's minimum level if there are
    /// no enforced claims.
    fn calculate_required_level(&self, element_id: ElementID) -> IndexedPowerLevel {
        let minimum_level = self.minimum_level(element_id);
        self.all_dependencies_for_required_element(element_id)
            .filter(|dep| self.is_dependency_enforced(dep))
            .map(|dep| dep.requires.level)
            .max()
            .unwrap_or(minimum_level)
    }

    /// Given a set of dependencies, filter out any redundant dependencies.
    /// A dependency is redundant if there exists another dependency between
    /// the *same pair of elements* at an *equal or higher level*.
    fn filter_out_redundant_dependencies(
        &self,
        mut dependencies: Vec<Dependency>,
    ) -> Vec<Dependency> {
        let mut essential_deps: Vec<Dependency> = Vec::new();
        let mut observed_pairs: HashSet<(Option<ElementID>, ElementID)> = HashSet::new();
        dependencies.sort_unstable_by_key(|dep| {
            (
                dep.dependent.as_ref().map(|d| d.element_id),
                dep.requires.element_id,
                usize::MAX - dep.requires.level.index,
            )
        });
        for dep in dependencies {
            let element_pair =
                (dep.dependent.as_ref().map(|d| d.element_id), dep.requires.element_id);
            if observed_pairs.insert(element_pair) {
                essential_deps.push(dep);
            }
        }
        essential_deps
    }

    /// Creates a new lease for the given targets along with all
    /// claims necessary to satisfy this lease and adds them as pending claims.
    /// Returns the new lease and the Vec of dependencies claimed.
    fn create_lease_and_claim_dependencies(
        &mut self,
        lease_name: Option<&str>,
        targets: Vec<ElementLevel>,
        lease_control: zx::Koid,
    ) -> (Lease, Vec<Dependency>) {
        log::debug!("create_lease_and_claim_dependencies({targets:?})");

        let lease = Lease::new(targets, lease_control);
        for target in &lease.targets {
            if let Some(element) = self.topology.get_element(&target.element_id) {
                self.topology.inspect().on_create_lease_and_claims(
                    element,
                    lease.id,
                    lease_name,
                    target.level.level,
                );
            }
        }
        self.leases.insert(lease.id, lease.clone());

        // Create all root and transitive dependencies.
        let mut dependencies = Vec::new();
        for target in &lease.targets {
            dependencies.push(Dependency::new_root(target.clone()));
            dependencies.extend(self.topology.all_direct_and_indirect_dependencies(target));
        }
        // Filter dependencies down to only the essential (i.e. non-redundant) dependencies.
        let deps_claimed = self.filter_out_redundant_dependencies(dependencies);
        for dep in &deps_claimed {
            self.add_pending_claim(dep.clone(), lease.id);
        }
        (lease, deps_claimed)
    }

    /// Drops an existing lease, and initiates process of releasing all
    /// associated claims, and transitions status to PoweringDown.
    fn drop_and_mark_powering_down(&mut self, lease_id: LeaseID) -> Result<Lease, Error> {
        log::debug!("drop_and_mark_powering_down(lease:{lease_id})");
        let lease =
            self.leases.get(&lease_id).cloned().ok_or_else(|| anyhow!("{lease_id} not found"))?;
        self.lease_status.update(&lease_id, LeaseStatus::PoweringDown);
        for target in &lease.targets {
            if let Some(element) = self.topology.get_element(&target.element_id) {
                self.topology.inspect().on_update_lease_status(
                    &element,
                    &lease,
                    &LeaseStatus::PoweringDown,
                );
            }
        }
        // Pending claims should be dropped immediately.
        self.drop_all_pending_for_lease(lease.id);

        // Claims should be marked to deactivate in an orderly sequence.
        log::debug!(
            "drop_and_mark_powering_down(lease:{lease_id}): marking active claims to deactivate"
        );
        self.mark_to_deactivate(lease.id);
        Ok(lease)
    }

    #[cfg(test)]
    fn get_lease_status(&self, lease_id: &LeaseID) -> Option<LeaseStatus> {
        self.lease_status.get(lease_id)
    }

    fn watch_lease_status(&mut self, lease_id: &LeaseID) -> UnboundedReceiver<Option<LeaseStatus>> {
        self.lease_status.subscribe(lease_id)
    }
}

/// Tracks the sets of leases claiming a specific [`Dependency`].
///
/// Claims on a [`Dependency`] are partitioned into three mutually exclusive states:
/// - `pending`: Waiting for prerequisite dependencies to be satisfied; not enforced.
/// - `active`: Lease is open and claim is enforced, affecting the required level.
/// - `deactivating`: Lease was dropped, but the claim is still enforced until dependent
///   elements finish powering down.
///
/// An **enforced** claim (`active or deactivating`) requires an element to be at least the
/// required level.
#[derive(Debug, Clone, Default)]
struct Claims {
    pending: FxHashSet<LeaseID>,
    active: FxHashSet<LeaseID>,
    deactivating: FxHashSet<LeaseID>,
}

impl Claims {
    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.active.is_empty() && self.deactivating.is_empty()
    }

    /// Returns true if any lease other than `lease_id` has an active claim.
    fn has_other_active(&self, lease_id: &LeaseID) -> bool {
        let active_count = self.active.len();
        if self.active.contains(lease_id) { active_count > 1 } else { active_count > 0 }
    }

    fn all_leases(&self) -> impl Iterator<Item = &LeaseID> {
        self.pending.iter().chain(self.active.iter()).chain(self.deactivating.iter())
    }
}

impl Catalog {
    fn update_claim_trace_counters(&self) {
        fuchsia_trace::counter!(
            c"power-broker", c"claims_pending", 0,
            "claims" => self.pending_claims_count as u32
        );
        fuchsia_trace::counter!(
            c"power-broker", c"claims_activated", 0,
            "claims" => self.enforced_claims_count as u32
        );
    }

    /// Registers a `(Dependency, LeaseID)` claim in `pending` if it is not already tracked.
    fn add_pending_claim(&mut self, dep: Dependency, lease_id: LeaseID) {
        match self.claims.get_mut(&dep) {
            Some(entry) => {
                // An enforced claim must not be moved back to `pending` here, and a claim that is
                // already pending needs no further bookkeeping.
                if entry.active.contains(&lease_id)
                    || entry.deactivating.contains(&lease_id)
                    || !entry.pending.insert(lease_id)
                {
                    return;
                }
            }
            None => {
                let mut entry = Claims::default();
                entry.pending.insert(lease_id);
                self.claims.insert(dep.clone(), entry);
                if dep.dependent.is_none() {
                    self.root_deps_by_required_element
                        .entry(dep.requires.element_id)
                        .or_default()
                        .insert(dep.clone());
                }
            }
        }
        self.lease_dependencies.entry(lease_id).or_default().insert(dep);
        self.pending_claims_count += 1;
        fuchsia_trace::counter!(
            c"power-broker", c"claims_pending", 0,
            "claims" => self.pending_claims_count as u32
        );
    }

    /// Transitions a claim from `pending` to `active`.
    fn activate_claim(&mut self, dep: &Dependency, lease_id: LeaseID) {
        log::debug!("activate_claim: {lease_id}:{dep}");
        let Some(entry) = self.claims.get_mut(dep) else {
            return;
        };
        if entry.pending.remove(&lease_id) {
            self.pending_claims_count -= 1;
            if entry.active.insert(lease_id) {
                self.enforced_claims_count += 1;
            }
            self.update_claim_trace_counters();
        }
    }

    /// Transitions an open lease's claim from `active` back to `pending`.
    fn deactivate_claim(&mut self, dep: &Dependency, lease_id: LeaseID) {
        log::debug!("deactivate_claim: {lease_id}:{dep}");
        let Some(entry) = self.claims.get_mut(dep) else {
            return;
        };
        if entry.active.remove(&lease_id) {
            self.enforced_claims_count -= 1;
            if entry.pending.insert(lease_id) {
                self.pending_claims_count += 1;
            }
            self.update_claim_trace_counters();
        }
    }

    /// Removes a `(Dependency, LeaseID)` claim from whichever state it occupies and cleans up
    /// unused entries.
    fn drop_claim(&mut self, dep: &Dependency, lease_id: LeaseID) {
        log::debug!("drop_claim: {lease_id}:{dep}");
        let mut changed = false;
        if let Some(entry) = self.claims.get_mut(dep) {
            debug_assert!(
                !(entry.active.contains(&lease_id) && entry.deactivating.contains(&lease_id)),
                "claim {lease_id}:{dep} must not be both active and deactivating"
            );
            if entry.pending.remove(&lease_id) {
                self.pending_claims_count -= 1;
                changed = true;
            }
            // `active` and `deactivating` are mutually exclusive, so at most one removal succeeds
            // and the enforced count is decremented at most once.
            if entry.active.remove(&lease_id) || entry.deactivating.remove(&lease_id) {
                self.enforced_claims_count -= 1;
                changed = true;
            }
            if entry.is_empty() {
                self.claims.remove(dep);
                if dep.dependent.is_none() {
                    if let Some(deps) =
                        self.root_deps_by_required_element.get_mut(&dep.requires.element_id)
                    {
                        deps.remove(dep);
                        if deps.is_empty() {
                            self.root_deps_by_required_element.remove(&dep.requires.element_id);
                        }
                    }
                }
            }
        }
        if let Some(deps) = self.lease_dependencies.get_mut(&lease_id) {
            deps.remove(dep);
            if deps.is_empty() {
                self.lease_dependencies.remove(&lease_id);
            }
        }
        if changed {
            self.update_claim_trace_counters();
        }
    }

    /// Removes all `pending` claims associated with `lease_id`.
    fn drop_all_pending_for_lease(&mut self, lease_id: LeaseID) {
        let Some(deps) = self.lease_dependencies.get(&lease_id).cloned() else {
            return;
        };
        for dep in deps {
            if self.claims.get(&dep).is_some_and(|entry| entry.pending.contains(&lease_id)) {
                self.drop_claim(&dep, lease_id);
            }
        }
    }

    /// Transitions all `active` claims for `lease_id` to `deactivating`, returning the affected
    /// dependencies. The enforced claim count is unchanged, as claims in both states are enforced.
    fn mark_to_deactivate(&mut self, lease_id: LeaseID) -> Vec<Dependency> {
        let mut marked = Vec::new();
        if let Some(deps) = self.lease_dependencies.get(&lease_id) {
            for dep in deps {
                if let Some(entry) = self.claims.get_mut(dep) {
                    if entry.active.remove(&lease_id) {
                        entry.deactivating.insert(lease_id);
                        marked.push(dep.clone());
                    }
                }
            }
        }
        log::debug!(
            "marking claims to deactivate for lease {lease_id}: [{}]",
            marked.iter().join(", ")
        );
        marked
    }

    fn is_claim_deactivating(&self, dep: &Dependency, lease_id: LeaseID) -> bool {
        self.claims.get(dep).is_some_and(|entry| entry.deactivating.contains(&lease_id))
    }

    fn is_claim_enforced(&self, dep: &Dependency, lease_id: LeaseID) -> bool {
        self.claims.get(dep).is_some_and(|entry| {
            entry.active.contains(&lease_id) || entry.deactivating.contains(&lease_id)
        })
    }

    fn is_dependency_enforced(&self, dep: &Dependency) -> bool {
        self.claims
            .get(dep)
            .is_some_and(|entry| !entry.active.is_empty() || !entry.deactivating.is_empty())
    }

    /// Asserts the invariants of the claim bookkeeping: claim states are mutually exclusive,
    /// `claims` and `lease_dependencies` agree with each other, empty entries are cleaned up, and
    /// the claim counts match the tracked claims.
    #[cfg(test)]
    fn assert_claims_consistent(&self) {
        let mut pending_count = 0;
        let mut enforced_count = 0;
        for (dep, entry) in &self.claims {
            assert!(!entry.is_empty(), "claims entry for {dep} should have been removed");
            for lease_id in entry.all_leases() {
                let states = usize::from(entry.pending.contains(lease_id))
                    + usize::from(entry.active.contains(lease_id))
                    + usize::from(entry.deactivating.contains(lease_id));
                assert_eq!(states, 1, "claim {lease_id}:{dep} must be in exactly one state");
                assert!(
                    self.lease_dependencies.get(lease_id).is_some_and(|deps| deps.contains(dep)),
                    "claim {lease_id}:{dep} is missing from lease_dependencies"
                );
            }
            pending_count += entry.pending.len();
            enforced_count += entry.active.len() + entry.deactivating.len();
        }
        assert_eq!(self.pending_claims_count, pending_count, "pending claim count is stale");
        assert_eq!(self.enforced_claims_count, enforced_count, "enforced claim count is stale");
        for (lease_id, deps) in &self.lease_dependencies {
            assert!(
                !deps.is_empty(),
                "lease_dependencies entry for {lease_id} should have been removed"
            );
            for dep in deps {
                assert!(
                    self.claims.get(dep).is_some_and(|entry| entry.all_leases().contains(lease_id)),
                    "lease_dependencies has {lease_id}:{dep} with no matching claim"
                );
            }
        }
        let mut root_deps = 0;
        for (element_id, deps) in &self.root_deps_by_required_element {
            assert!(
                !deps.is_empty(),
                "root_deps_by_required_element entry for {element_id} should have been removed"
            );
            for dep in deps {
                assert!(dep.dependent.is_none(), "{dep} is not a root dependency");
                assert_eq!(dep.requires.element_id, *element_id, "{dep} is indexed incorrectly");
                assert!(self.claims.contains_key(dep), "indexed {dep} has no claims");
            }
            root_deps += deps.len();
        }
        assert_eq!(
            root_deps,
            self.claims.keys().filter(|dep| dep.dependent.is_none()).count(),
            "root_deps_by_required_element does not match the claimed root dependencies"
        );
    }

    #[cfg(test)]
    fn pending_for_lease(&self, lease_id: LeaseID) -> Vec<Dependency> {
        let mut deps_res = Vec::new();
        if let Some(deps) = self.lease_dependencies.get(&lease_id) {
            for dep in deps {
                if self.claims.get(dep).is_some_and(|entry| entry.pending.contains(&lease_id)) {
                    deps_res.push(dep.clone());
                }
            }
        }
        deps_res
    }

    fn enforced_for_lease(&self, lease_id: LeaseID) -> impl Iterator<Item = &Dependency> {
        self.lease_dependencies
            .get(&lease_id)
            .into_iter()
            .flat_map(|deps| deps.iter())
            .filter(move |dep| self.is_claim_enforced(dep, lease_id))
    }

    /// Returns the deactivating claims for `lease_id`, in a deterministic order.
    fn deactivating_for_lease(&self, lease_id: LeaseID) -> Vec<(Dependency, LeaseID)> {
        let mut claims = Vec::new();
        if let Some(deps) = self.lease_dependencies.get(&lease_id) {
            for dep in deps {
                if self.is_claim_deactivating(dep, lease_id) {
                    claims.push((dep.clone(), lease_id));
                }
            }
        }
        claims.sort_unstable();
        claims
    }

    /// Returns the pending claims on dependencies requiring `element_id`, in a deterministic
    /// order.
    fn pending_for_required_element(&self, element_id: ElementID) -> Vec<(Dependency, LeaseID)> {
        let mut claims = Vec::new();
        for dep in self.all_dependencies_for_required_element(element_id) {
            if let Some(entry) = self.claims.get(dep) {
                for lease_id in &entry.pending {
                    claims.push((dep.clone(), *lease_id));
                }
            }
        }
        claims.sort_unstable();
        claims
    }

    /// Returns the deactivating claims on dependencies whose dependent is `element_id`, in a
    /// deterministic order.
    fn deactivating_for_dependent_element(
        &self,
        element_id: ElementID,
    ) -> Vec<(Dependency, LeaseID)> {
        let mut claims = Vec::new();
        for dep in self.topology.dependencies_for_dependent_element(element_id) {
            if let Some(entry) = self.claims.get(dep) {
                for lease_id in &entry.deactivating {
                    claims.push((dep.clone(), *lease_id));
                }
            }
        }
        claims.sort_unstable();
        claims
    }
}

trait Inspectable {
    type Value;
    fn track_inspect_with(&self, value: Self::Value, parent: &INode) -> Box<dyn IType>;
}

impl Inspectable for &ElementID {
    type Value = IndexedPowerLevel;
    fn track_inspect_with(&self, value: Self::Value, parent: &INode) -> Box<dyn IType> {
        Box::new(parent.create_uint(self.to_string(), value.level.into()))
    }
}

impl Inspectable for &LeaseID {
    type Value = LeaseStatus;
    fn track_inspect_with(&self, value: Self::Value, parent: &INode) -> Box<dyn IType> {
        Box::new(parent.create_string(format!("{self}"), format!("{:?}", value)))
    }
}

#[derive(Debug)]
struct Data<V: Clone + PartialEq> {
    value: Option<V>,
    senders: Vec<UnboundedSender<Option<V>>>,
    _inspect: Option<Box<dyn IType>>,
}

impl<V: Clone + PartialEq> Default for Data<V> {
    fn default() -> Self {
        Data { value: None, senders: Vec::new(), _inspect: None }
    }
}

/// SubscribeMap is a wrapper around a HashMap that stores values V by key K
/// and allows subscribers to register a channel on which they will receive
/// updates whenever the value stored changes.
#[derive(Debug)]
struct SubscribeMap<K: Clone + Hash + Eq, V: Clone + PartialEq> {
    values: HashMap<K, Data<V>>,
    inspect: Option<INode>,
}

impl<K: Clone + Hash + Eq, V: Clone + PartialEq> SubscribeMap<K, V> {
    fn new(inspect: Option<INode>) -> Self {
        SubscribeMap { values: HashMap::new(), inspect }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.values.get(key).map(|d| d.value.clone()).flatten()
    }

    // update updates the value for key.
    // Returns previous value, if any.
    fn update<'a>(&mut self, key: &'a K, value: V) -> Option<V>
    where
        &'a K: Inspectable<Value = V>,
        V: Copy,
    {
        let previous = self.get(key);
        // If the value hasn't changed, this is a no-op, return.
        if previous.as_ref() == Some(&value) {
            return previous;
        }
        let mut senders = Vec::new();
        if let Some(Data { value: _, senders: old_senders, _inspect: _ }) = self.values.remove(&key)
        {
            // Prune invalid senders.
            for sender in old_senders {
                if let Err(err) = sender.unbounded_send(Some(value.clone())) {
                    if err.is_disconnected() {
                        continue;
                    }
                }
                senders.push(sender);
            }
        }
        let _inspect = self.inspect.as_mut().map(|inspect| key.track_inspect_with(value, &inspect));
        let value = Some(value);
        self.values.insert(key.clone(), Data { value, senders, _inspect });
        previous
    }

    fn subscribe(&mut self, key: &K) -> UnboundedReceiver<Option<V>> {
        let (sender, receiver) = unbounded::<Option<V>>();
        sender.unbounded_send(self.get(key)).expect("initial send should not fail");
        self.values.entry(key.clone()).or_default().senders.push(sender);
        receiver
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        self.values.remove(key).and_then(|data| data.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyProperty, PropertyAssertion, assert_data_tree};
    use fidl_fuchsia_power_broker::{BinaryPowerLevel, DependencyToken};
    use fuchsia_inspect::hierarchy::DiagnosticsHierarchy;

    use power_broker_client::BINARY_POWER_LEVELS;

    // Convenience aliases.
    const OFF: IndexedPowerLevel =
        IndexedPowerLevel { level: BinaryPowerLevel::Off.into_primitive(), index: 0 };
    const ON: IndexedPowerLevel =
        IndexedPowerLevel { level: BinaryPowerLevel::On.into_primitive(), index: 1 };

    const ZERO: IndexedPowerLevel = IndexedPowerLevel::from_same_level_and_index(0);
    const ONE: IndexedPowerLevel = IndexedPowerLevel::from_same_level_and_index(1);
    const TWO: IndexedPowerLevel = IndexedPowerLevel::from_same_level_and_index(2);

    #[track_caller]
    fn assert_element_cleaned_up(broker: &Broker, element_id: ElementID) {
        assert!(
            !broker.catalog.topology.element_exists(element_id),
            "topology.elements not cleaned up"
        );
        assert_eq!(broker.in_transition.get(&element_id), None, "in_transition not cleaned up");
        assert!(!broker.current.contains_key(&element_id), "current not cleaned up");
        assert_eq!(broker.required.get(&element_id), None, "required not cleaned up");
        assert_eq!(broker.lease_counter.get(&element_id), None, "lease_counter not cleaned up");
    }

    #[track_caller]
    fn assert_lease_cleaned_up(catalog: &Catalog, lease_id: LeaseID) {
        assert!(!catalog.leases.contains_key(&lease_id), "{lease_id} still in catalog.leases");
        assert!(
            catalog.lease_status.get(&lease_id).is_none(),
            "{lease_id} still in catalog.lease_status"
        );
        assert_eq!(catalog.enforced_for_lease(lease_id).count(), 0, "claims.enforced not empty");
        assert!(catalog.has_no_remaining_claims(lease_id), "claims not cleaned up");
    }

    #[track_caller]
    fn assert_events_recorded_in_order_helper(
        hierarchy: &DiagnosticsHierarchy,
        expected_events: &[(&'static str, Vec<(&'static str, Box<dyn PropertyAssertion>)>)],
    ) {
        let events_node = hierarchy
            .get_child_by_path(&["test", "topology", "events"])
            .expect("events node found");

        let mut expected_idx = 0;

        for child_node in events_node.children.iter().rev() {
            if expected_idx >= expected_events.len() {
                break;
            }
            let (expected_name, expected_properties) = &expected_events[expected_idx];
            if let Some(event_node) = child_node.get_child(expected_name) {
                let matches = expected_properties.iter().all(|(prop_name, assertion)| {
                    if let Some(property) = event_node.get_property(prop_name) {
                        assertion.run(property).is_ok()
                    } else {
                        false
                    }
                });
                if matches {
                    expected_idx += 1;
                }
            }
        }

        if expected_idx != expected_events.len() {
            println!("--- INSPECT EVENTS RECORDED ---");
            for child_node in events_node.children.iter().rev() {
                println!("Event: {:?}", child_node);
            }
            println!("---------------------------------");
        }

        assert_eq!(
            expected_idx,
            expected_events.len(),
            "Failed to find all expected events in chronological order. Found {}/{} expected events.",
            expected_idx,
            expected_events.len()
        );
    }

    macro_rules! assert_events_recorded_in_order {
        ($hierarchy:expr, [ $( { $event:ident: { $( $prop:ident: $val:expr $(,)? )* } $(,)? } $(,)? )* ]) => {{
            let expected = vec![
                $(
                    (
                        stringify!($event),
                        vec![
                            $(
                                (stringify!($prop), Box::new($val) as Box<dyn PropertyAssertion>),
                            )*
                        ]
                    ),
                )*
            ];
            assert_events_recorded_in_order_helper($hierarchy, &expected);
        }};
    }

    #[fuchsia::test]
    fn test_binary_satisfy_power_level() {
        for (level, required, want) in
            [(OFF, ON, false), (OFF, OFF, true), (ON, OFF, true), (ON, ON, true)]
        {
            let got = level.satisfies(required);
            assert_eq!(
                got, want,
                "{:?}.satisfies({:?}) = {:?}, want {:?}",
                level, required, got, want
            );
        }
    }

    #[fuchsia::test]
    fn test_user_defined_satisfy_power_level() {
        for (level, required, want) in [
            (0, 1, false),
            (0, 0, true),
            (1, 0, true),
            (1, 1, true),
            (255, 0, true),
            (255, 1, true),
            (255, 255, true),
            (1, 255, false),
            (35, 36, false),
            (35, 35, true),
        ] {
            let level = IndexedPowerLevel::from_same_level_and_index(level);
            let required = IndexedPowerLevel::from_same_level_and_index(required);
            let got = level.satisfies(required);
            assert_eq!(
                got, want,
                "{:?}.satisfies({:?}) = {:?}, want {:?}",
                level, required, got, want
            );
        }
    }

    #[fuchsia::test]
    fn test_option_satisfy_power_level() {
        for (level, required, want) in [
            (None, 0, false),
            (None, 1, false),
            (Some(0), 1, false),
            (Some(0), 0, true),
            (Some(1), 0, true),
            (Some(1), 1, true),
            (Some(255), 0, true),
            (Some(255), 1, true),
            (Some(255), 255, true),
            (Some(1), 255, false),
            (Some(35), 36, false),
            (Some(35), 35, true),
        ] {
            let level = level.map(|l| IndexedPowerLevel::from_same_level_and_index(l));
            let required = IndexedPowerLevel::from_same_level_and_index(required);
            let got = level.satisfies(required);
            assert_eq!(
                got, want,
                "{:?}.satisfies({:?}) = {:?}, want {:?}",
                level, required, got, want
            );
        }
    }

    #[fuchsia::test]
    fn test_levels() {
        let mut levels = SubscribeMap::<ElementID, IndexedPowerLevel>::new(None);

        let element_a = ElementID::new(1);
        let element_b = ElementID::new(2);
        levels.update(&element_a, ON);
        assert_eq!(levels.get(&element_a), Some(ON));
        assert_eq!(levels.get(&element_b), None);

        levels.update(&element_a, OFF);
        levels.update(&element_b, ON);
        assert_eq!(levels.get(&element_a), Some(OFF));
        assert_eq!(levels.get(&element_b), Some(ON));

        let element_ud1 = ElementID::new(3);
        let element_ud2 = ElementID::new(4);
        levels.update(&element_ud1, IndexedPowerLevel::from_same_level_and_index(145));
        assert_eq!(
            levels.get(&element_ud1),
            Some(IndexedPowerLevel::from_same_level_and_index(145))
        );
        assert_eq!(levels.get(&element_ud2), None);

        levels.update(&element_a, ON);
        levels.remove(&element_b);
        assert_eq!(levels.get(&element_b), None);
    }

    #[fuchsia::test]
    fn test_levels_subscribe() {
        let mut levels = SubscribeMap::<ElementID, IndexedPowerLevel>::new(None);

        let element_a = ElementID::new(1);
        let element_b = ElementID::new(2);
        let mut receiver_a = levels.subscribe(&element_a);
        let mut receiver_b = levels.subscribe(&element_b);

        levels.update(&element_a, ON);
        assert_eq!(levels.get(&element_a), Some(ON));
        assert_eq!(levels.get(&element_b), None);

        levels.update(&element_a, OFF);
        levels.update(&element_b, ON);
        assert_eq!(levels.get(&element_a), Some(OFF));
        assert_eq!(levels.get(&element_b), Some(ON));

        let mut received_a = Vec::new();
        while let Ok(level) = receiver_a.try_recv() {
            received_a.push(level)
        }
        assert_eq!(received_a, vec![None, Some(ON), Some(OFF)]);
        let mut received_b = Vec::new();
        while let Ok(level) = receiver_b.try_recv() {
            received_b.push(level)
        }
        assert_eq!(received_b, vec![None, Some(ON)]);
    }

    fn create_test_dependency(
        dependent_element_id: ElementID,
        dependent_element_level: fpb::PowerLevel,
        requires_element_id: ElementID,
        requires_element_level: fpb::PowerLevel,
    ) -> Dependency {
        Dependency {
            dependent: Some(ElementLevel {
                element_id: dependent_element_id,
                level: IndexedPowerLevel::from_same_level_and_index(dependent_element_level),
            }),
            requires: ElementLevel {
                element_id: requires_element_id,
                level: IndexedPowerLevel::from_same_level_and_index(requires_element_level),
            },
        }
    }

    // When claims are added, activated, marked to deactivate, and dropped, the Catalog's counts
    // and indexes should track each transition, and dropping the last claim on a dependency
    // should leave no entry behind.
    #[fuchsia::test]
    fn test_catalog_claims_add_remove() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut catalog = Catalog::new(&inspect.root().create_child("test"));

        let element_a = catalog.topology.add_element("A", &[0, 1, 2]).unwrap();
        AddElementInspectWriter::new(element_a).commit(&mut catalog.topology);
        let element_b = catalog.topology.add_element("B", &[0, 1, 2]).unwrap();
        AddElementInspectWriter::new(element_b).commit(&mut catalog.topology);
        let dep_a_1_b_1 = create_test_dependency(element_a, 1, element_b, 1);
        let dep_a_2_b_2 = create_test_dependency(element_a, 2, element_b, 2);
        catalog
            .topology
            .add_dependency(
                &dep_a_1_b_1,
                OnRequiredElementRemoval::MakeUnsatisfiable,
                &mut EagerInspectWriter,
            )
            .unwrap();
        catalog
            .topology
            .add_dependency(
                &dep_a_2_b_2,
                OnRequiredElementRemoval::MakeUnsatisfiable,
                &mut EagerInspectWriter,
            )
            .unwrap();
        let lease_1 = LeaseID(10);
        let lease_2 = LeaseID(20);

        catalog.add_pending_claim(dep_a_1_b_1.clone(), lease_1);
        catalog.add_pending_claim(dep_a_2_b_2.clone(), lease_1);
        catalog.add_pending_claim(dep_a_1_b_1.clone(), lease_2);

        assert_eq!(catalog.pending_claims_count, 3);
        assert_eq!(catalog.enforced_claims_count, 0);
        catalog.assert_claims_consistent();

        catalog.activate_claim(&dep_a_1_b_1, lease_1);
        catalog.activate_claim(&dep_a_2_b_2, lease_1);
        assert_eq!(catalog.pending_claims_count, 1);
        assert_eq!(catalog.enforced_claims_count, 2);
        catalog.assert_claims_consistent();

        assert!(catalog.is_claim_enforced(&dep_a_1_b_1, lease_1));
        assert!(!catalog.is_claim_enforced(&dep_a_1_b_1, lease_2));

        let marked = catalog.mark_to_deactivate(lease_1);
        assert_eq!(marked.len(), 2);
        let deactivating_claims = catalog.deactivating_for_dependent_element(element_a);
        assert_eq!(deactivating_claims.len(), 2);
        assert!(deactivating_claims.contains(&(dep_a_1_b_1.clone(), lease_1)));
        assert!(deactivating_claims.contains(&(dep_a_2_b_2.clone(), lease_1)));
        catalog.assert_claims_consistent();

        catalog.drop_claim(&dep_a_1_b_1, lease_1);
        catalog.drop_claim(&dep_a_2_b_2, lease_1);
        assert_eq!(catalog.enforced_claims_count, 0);
        assert_eq!(catalog.pending_claims_count, 1);
        catalog.assert_claims_consistent();

        catalog.drop_claim(&dep_a_1_b_1, lease_2);
        assert_eq!(catalog.pending_claims_count, 0);
        assert_eq!(catalog.enforced_claims_count, 0);
        assert!(catalog.claims.is_empty());
        assert!(catalog.lease_dependencies.is_empty());
        catalog.assert_claims_consistent();
    }

    // When claim transitions are requested out of order, the claim bookkeeping should hold these
    // invariants:
    //
    //   1. A claim is in exactly one of `pending`, `active`, or `deactivating`.
    //   2. `activate_claim` only promotes a claim that is already pending. A lease that never
    //      registered a claim is not enforced by activating it.
    //   3. `add_pending_claim` never demotes an enforced claim back to `pending`.
    //   4. `deactivating` is terminal. Nothing moves a claim back out of it until it is dropped,
    //      and it stays enforced the whole time.
    //
    // `pending_claims_count` and `enforced_claims_count` must stay accurate across every no-op
    // above, and `assert_claims_consistent` rechecks invariant 1 (plus the `claims` and
    // `lease_dependencies` cross-references) after each step.
    #[fuchsia::test]
    fn test_catalog_claims_invariants() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut catalog = Catalog::new(&inspect.root().create_child("test"));
        let element_a = ElementID::new(1);
        let element_b = ElementID::new(2);
        let dep = create_test_dependency(element_a, 1, element_b, 1);
        let lease_1 = LeaseID(10);
        let lease_2 = LeaseID(20);

        // Setup: lease_1 has an active claim on `dep`.
        catalog.add_pending_claim(dep.clone(), lease_1);
        catalog.activate_claim(&dep, lease_1);
        assert_eq!(catalog.pending_claims_count, 0);
        assert_eq!(catalog.enforced_claims_count, 1);
        catalog.assert_claims_consistent();

        // Invariant 2: lease_2 never called `add_pending_claim`, so activating it must not enforce
        // a claim, even though `dep` already has an entry from lease_1. It must also not gain a
        // `lease_dependencies` entry, which would outlive the claim it never had.
        catalog.activate_claim(&dep, lease_2);
        let entry = catalog.claims.get(&dep).unwrap();
        assert!(!entry.active.contains(&lease_2));
        assert!(catalog.lease_dependencies.get(&lease_2).is_none());
        catalog.assert_claims_consistent();

        // Invariant 3: re-adding an already-active claim must leave it active rather than also
        // marking it pending, which would both break exclusivity and double count it.
        catalog.add_pending_claim(dep.clone(), lease_1);
        let entry = catalog.claims.get(&dep).unwrap();
        assert!(!entry.pending.contains(&lease_1));
        assert!(entry.active.contains(&lease_1));
        assert_eq!(catalog.pending_claims_count, 0);
        assert_eq!(catalog.enforced_claims_count, 1);
        catalog.assert_claims_consistent();

        // Invariant 4: once marked to deactivate, none of the three transitions may move the claim
        // out of `deactivating`. Releasing it early would drop the required level of an element
        // that has not finished powering down.
        catalog.mark_to_deactivate(lease_1);
        catalog.deactivate_claim(&dep, lease_1);
        catalog.activate_claim(&dep, lease_1);
        catalog.add_pending_claim(dep.clone(), lease_1);
        let entry = catalog.claims.get(&dep).unwrap();
        assert!(!entry.pending.contains(&lease_1));
        assert!(!entry.active.contains(&lease_1));
        assert!(entry.deactivating.contains(&lease_1));
        assert_eq!(catalog.pending_claims_count, 0);
        // Still enforced: `deactivating` claims count as enforced until they are dropped.
        assert_eq!(catalog.enforced_claims_count, 1);
        catalog.assert_claims_consistent();
    }

    // When a dependency is added beneath an element that a powering-down lease still holds an
    // enforced claim on, that lease should not take a claim on the newly required element.
    // `drop_lease` has already dropped the lease's pending claims and marked the rest
    // deactivating, so a claim added afterwards would be pending on a lease that will never
    // process pending claims again: nothing would drop it, the new element would stay required,
    // and the lease would never vacate.
    #[fuchsia::test]
    fn test_add_dependency_while_lease_powering_down() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let token_b = DependencyToken::create();
        let token_c = DependencyToken::create();

        let element_c = broker
            .add_element("C", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_c,
                token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        let element_b = broker
            .add_element("B", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        let element_a = broker
            .add_element(
                "A",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        // Acquire lease on A(ON); B and A turn ON.
        let lease_a = broker.acquire_lease(element_a, ON, zx::Koid::from_raw(1)).unwrap();
        broker.update_current_level(element_b, ON);
        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_lease_status(lease_a.id), Some(LeaseStatus::Satisfied));

        // Drop lease_a while A has not yet completed transitioning to OFF.
        broker.drop_lease(lease_a.id).unwrap();
        assert_eq!(broker.get_lease_status(lease_a.id), Some(LeaseStatus::PoweringDown));

        // Dynamically add B(ON) -> C(ON) while lease_a is powering down. `prepare_add_dependency`
        // returns a provisional lease that holds C up while the edge is added, so that adding the
        // edge cannot strand B(ON) above an unsatisfied C.
        let provisional_lease = broker
            .prepare_add_dependency(
                element_b,
                &fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        broker.update_current_level(element_c, ON);
        broker
            .add_dependency(
                element_b,
                fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                },
                &mut EagerInspectWriter,
            )
            .unwrap();

        // lease_a is powering down, so the new dependency must not have been claimed on its
        // behalf. Such a claim would be pending on a lease that has already dropped its pending
        // claims, leaving C required forever.
        assert!(
            !broker
                .catalog
                .lease_dependencies
                .get(&lease_a.id)
                .is_some_and(|deps| deps.iter().any(|d| d.requires.element_id == element_c)),
            "powering-down lease must not claim the newly added dependency"
        );

        broker.drop_lease(provisional_lease.id).unwrap();

        // Complete power-down of A and B to OFF.
        broker.update_current_level(element_a, OFF);
        broker.update_current_level(element_b, OFF);

        // C is left at OFF because the only claim on it belonged to the provisional lease, and
        // lease_a vacates cleanly once its own claims drop.
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
        assert_lease_cleaned_up(&broker.catalog, lease_a.id);
    }

    // When an element is removed out from under a lease that is still powering down, the lease
    // should still vacate with no claims left behind, and the elements it held should return to
    // their minimum levels for later leases.
    #[fuchsia::test]
    fn test_disorderly_drop_while_lease_powering_down() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let token_a = DependencyToken::create();
        let token_b = DependencyToken::create();

        let element_a = broker
            .add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        let element_b = broker
            .add_element("B", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        let element_x = broker
            .add_element(
                "X",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");

        // Acquire lease_x1 on X(ON); A, B, and X turn ON.
        let lease_x1 = broker.acquire_lease(element_x, ON, zx::Koid::from_raw(1)).unwrap();
        broker.update_current_level(element_a, ON);
        broker.update_current_level(element_b, ON);
        broker.update_current_level(element_x, ON);
        assert_eq!(broker.get_lease_status(lease_x1.id), Some(LeaseStatus::Satisfied));

        // Drop lease_x1 while X stays in transition to OFF (so X->A and X->B are in powering_down).
        broker.drop_lease(lease_x1.id).unwrap();
        assert_eq!(broker.get_lease_status(lease_x1.id), Some(LeaseStatus::PoweringDown));

        // Remove element B while lease_x1 is powering down, causing a disorderly drop on B.
        broker.remove_element(&element_b);
        broker.update_current_level(element_a, OFF);

        // Complete X's transition to OFF. lease_x1 must be cleanly vacated with no leftover claims.
        broker.update_current_level(element_x, OFF);
        assert_lease_cleaned_up(&broker.catalog, lease_x1.id);

        // Acquire and drop a new lease on A(ON); A must cleanly return to required level OFF.
        let lease_a = broker.acquire_lease(element_a, ON, zx::Koid::from_raw(2)).unwrap();
        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_lease_status(lease_a.id), Some(LeaseStatus::Satisfied));
        broker.drop_lease(lease_a.id).unwrap();
        assert_eq!(broker.get_required_level(&element_a), Some(OFF));
    }

    // When a lease's dependencies include several between the same pair of elements, only the
    // one at the highest level should be kept. Dependencies between different pairs should all
    // be kept, even where one appears to subsume another transitively.
    #[fuchsia::test]
    fn test_filter_out_redundant_dependencies() {
        let inspect = fuchsia_inspect::Inspector::default();
        let broker = Broker::new(inspect.root().create_child("test"));

        let element_a = ElementID::new(1);
        let element_b = ElementID::new(2);
        let element_c = ElementID::new(3);
        let dep_a_1_b_1 = create_test_dependency(element_a, 1, element_b, 1);
        let dep_a_2_b_2 = create_test_dependency(element_a, 2, element_b, 2);
        let dep_a_1_c_1 = create_test_dependency(element_a, 1, element_c, 1);
        let dep_b_1_c_1 = create_test_dependency(element_b, 1, element_c, 1);
        let dep_a_2_c_2 = create_test_dependency(element_a, 2, element_c, 2);

        //  A     B
        //  1 ==> 1 (redundant with A@2=>B@2)
        //  2 ==> 2
        let essential_deps = broker
            .catalog
            .filter_out_redundant_dependencies(vec![dep_a_1_b_1.clone(), dep_a_2_b_2.clone()]);
        assert_eq!(essential_deps, vec![dep_a_2_b_2.clone()]);

        //  A     B     C
        //  1 ========> 1 (not redundant, not between same elements)
        //  2 ==> 2
        let essential_deps = broker
            .catalog
            .filter_out_redundant_dependencies(vec![dep_a_1_c_1.clone(), dep_a_2_b_2.clone()]);
        assert_eq!(essential_deps, vec![dep_a_2_b_2.clone(), dep_a_1_c_1.clone()]);

        //  A     B     C
        //  1 ==> 1 ==> 1 (not redundant, A@2=>C@2 cannot satisfy B@1=>C@1, not between same elements)
        //  2 ========> 2
        let essential_deps = broker.catalog.filter_out_redundant_dependencies(vec![
            dep_a_1_b_1.clone(),
            dep_b_1_c_1.clone(),
            dep_a_2_c_2.clone(),
        ]);
        assert_eq!(
            essential_deps,
            vec![dep_a_1_b_1.clone(), dep_a_2_c_2.clone(), dep_b_1_c_1.clone()]
        );
    }

    #[fuchsia::test]
    async fn test_initialize_current_and_broker_status() {
        let inspect = fuchsia_inspect::Inspector::default();
        let inspect_node = inspect.root().create_child("test");
        let mut broker = Broker::new(inspect_node);
        let latinum = broker
            .add_element(
                "Latinum",
                7,
                // Unsorted. The order declares the order of increasing power.
                vec![5, 2, 7],
                vec![],
            )
            .expect("add_element failed");
        assert_eq!(broker.lookup_name(latinum), "Latinum".to_string());
        assert_eq!(
            broker.get_current_level(&latinum),
            Some(IndexedPowerLevel { level: 7, index: 2 })
        );
        assert_eq!(
            broker.get_required_level(&latinum),
            Some(IndexedPowerLevel { level: 5, index: 0 })
        );

        assert_data_tree!(inspect, root: {
            test: {
                leases: {},
                topology: {
                    events: {
                        "0": {
                            "@time": AnyProperty,
                            add_element: {
                                element_id: *broker.get_unsatisfiable_element_id(),
                                current_level: "unset",
                                required_level: "unset",
                            }
                        },
                        "1": {
                            "@time": AnyProperty,
                            add_element: {
                                element_id: *latinum,
                                current_level: 7u64,
                                required_level: 5u64,
                            }
                        },
                    },
                    stats: contains {},
                    "fuchsia.inspect.Graph": {
                        topology: {
                            broker.get_unsatisfiable_element_id().to_string() => {
                                meta: {
                                    name: broker.get_unsatisfiable_element_name(),
                                    valid_levels: broker.get_unsatisfiable_element_levels(),
                                    required_level: "unset",
                                    current_level: "unset",
                                    leases: {}
                                },
                                relationships: {}
                            },
                            latinum.to_string() => {
                                meta: {
                                    name: "Latinum",
                                    valid_levels: vec![5u64, 2u64, 7u64],
                                    current_level: 7u64,
                                    required_level: 5u64,
                                    leases: {}
                                },
                                relationships: {},
                            },
                        },
        }}}});
    }

    #[fuchsia::test]
    async fn test_current_required_level_inspect() {
        let inspect = fuchsia_inspect::Inspector::default();
        let inspect_node = inspect.root().create_child("test");
        let mut broker = Broker::new(inspect_node);
        let latinum =
            broker.add_element("Latinum", 2, vec![0, 1, 2], vec![]).expect("add_element failed");
        assert_eq!(broker.get_current_level(&latinum), Some(TWO));
        assert_eq!(broker.get_required_level(&latinum), Some(ZERO));

        // Update current level to 0 to preserve ordering.
        broker.update_current_level(latinum, ZERO);

        // Update required level to 1.
        broker.update_required_level(latinum, ONE, &mut EagerInspectWriter);

        // Update required level to 1 again, should have no additional effect.
        broker.update_required_level(latinum, ONE, &mut EagerInspectWriter);

        // Update current level to 1.
        // This should drop the current required level to ZERO, because there
        // isn't any claim preserving the required level update.
        broker.update_current_level(latinum, ONE);

        // Update current level to 1 again, should have no additional effect.
        broker.update_current_level(latinum, ONE);

        assert_data_tree!(inspect, root: {
        test: {
            leases: {},
            topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        broker.get_unsatisfiable_element_id().to_string() => {
                            meta: {
                                name: broker.get_unsatisfiable_element_name(),
                                valid_levels: broker.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {},
                            },
                            relationships: {}
                        },
                        latinum.to_string() => {
                            meta: {
                                name: "Latinum",
                                valid_levels: vec![0u64, 1u64, 2u64],
                                current_level: 1u64,
                                required_level: 0u64,
                                leases: {},
                            },
                            relationships: {},
                        },
                    },
                },
                stats: contains {},
                events: {
                    "0": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *broker.get_unsatisfiable_element_id(),
                            current_level: "unset",
                            required_level: "unset",
                        }
                    },
                    "1": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *latinum,
                            current_level: 2u64,
                            required_level: 0u64,
                        }
                    },
                    "2": {
                        "@time": AnyProperty,
                        update_level: {
                            element_id: *latinum,
                            current_level: 0u64,
                        }
                    },
                    "3": {
                        "@time": AnyProperty,
                        update_level: {
                            element_id: *latinum,
                            required_level: 1u64,
                        }
                    },
                    "4": {
                        "@time": AnyProperty,
                        update_level: {
                            element_id: *latinum,
                            current_level: 1u64,
                            required_level: 0u64,
                        }
                    },
                },
            }}});
    }

    #[fuchsia::test]
    async fn test_add_element_dependency_never_and_unregistered() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let token_mithril = DependencyToken::create();
        let never_registered_token = DependencyToken::create();
        let mithril = broker
            .add_element("Mithril", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                mithril,
                token_mithril.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let v01: Vec<u64> = BINARY_POWER_LEVELS.iter().map(|&v| v as u64).collect();
        assert_data_tree!(inspect, root: {
            test: {
                leases: {},
                topology: {
                    events: contains {},
                    stats: contains {},
                    "fuchsia.inspect.Graph": {
                        topology: {
                            broker.get_unsatisfiable_element_id().to_string() => {
                                meta: {
                                    name: broker.get_unsatisfiable_element_name(),
                                    valid_levels: broker.get_unsatisfiable_element_levels(),
                                    required_level: "unset",
                                    current_level: "unset",
                                    leases: {}
                                },
                                relationships: {}
                            },
                            mithril.to_string() => {
                                meta: {
                                    name: "Mithril",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {},
                            },
                        },
                    },
                },
        }});

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    add_element: {
                        element_id: *broker.get_unsatisfiable_element_id(),
                        current_level: "unset",
                        required_level: "unset",
                    },
                },
                {
                    add_element: {
                        element_id: *mithril,
                        current_level: 0u64,
                        required_level: 0u64,
                    },
                },
            ]
        );

        // This should fail, because the token was never registered.
        let add_element_not_authorized_res = broker.add_element(
            "Silver",
            OFF.level,
            BINARY_POWER_LEVELS.to_vec(),
            vec![fpb::LevelDependency {
                dependent_level: Some(ON.level),
                requires_token: Some(
                    never_registered_token
                        .duplicate_handle(zx::Rights::SAME_RIGHTS)
                        .expect("dup failed"),
                ),
                requires_level_by_preference: Some(vec![ON.level]),
                ..Default::default()
            }],
        );
        assert!(matches!(add_element_not_authorized_res, Err(AddElementError::NotAuthorized)));

        // Add element with a valid token should succeed.
        let silver = broker
            .add_element(
                "Silver",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_mithril
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        assert_data_tree!(inspect, root: {
        test: {
            leases: {},
            topology: {
                events: contains {},
                stats: contains {},
                "fuchsia.inspect.Graph": {
                    topology: {
                        broker.get_unsatisfiable_element_id().to_string() => {
                            meta: {
                                name: broker.get_unsatisfiable_element_name(),
                                valid_levels: broker.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}
                        },
                        mithril.to_string() => {
                            meta: {
                                name: "Mithril",
                                valid_levels: v01.clone(),
                                current_level: OFF.level as u64,
                                required_level: OFF.level as u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                        silver.to_string() => {
                            meta: {
                                name: "Silver",
                                valid_levels: v01.clone(),
                                current_level: OFF.level as u64,
                                required_level: OFF.level as u64,
                                leases: {}
                            },
                            relationships: {
                                mithril.to_string() => {
                                    edge_id: AnyProperty,
                                    meta: {
                                      "1": {
                                          required_level: 1u64,
                                      },
                                    },
                                },
                            },
                        },
                    },
                }
            }}});

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    add_element: {
                        element_id: *broker.get_unsatisfiable_element_id(),
                        current_level: "unset",
                        required_level: "unset",
                    },
                },
                {
                    add_element: {
                        element_id: *mithril,
                        current_level: 0u64,
                        required_level: 0u64,
                    },
                },
                {
                    add_element: {
                        element_id: *silver,
                        current_level: 0u64,
                        required_level: 0u64,
                    },
                },
            ]
        );

        // Unregister token_mithril, then try to add again, which should fail.
        broker
            .unregister_dependency_token(
                mithril,
                token_mithril.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("unregister_dependency_token failed");

        let add_element_not_authorized_res: Result<ElementID, AddElementError> = broker
            .add_element(
                "Silver",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_mithril
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            );
        assert!(matches!(add_element_not_authorized_res, Err(AddElementError::NotAuthorized)));
    }

    #[fuchsia::test]
    async fn test_remove_element() {
        let inspect = fuchsia_inspect::Inspector::default();
        let inspect_node = inspect.root().create_child("test");
        let mut broker = Broker::new(inspect_node);
        let unobtanium = broker
            .add_element("Unobtainium", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        assert_eq!(broker.element_exists(unobtanium), true);
        let v01: Vec<u64> = BINARY_POWER_LEVELS.iter().map(|&v| v as u64).collect();
        assert_data_tree!(inspect, root: {
        test: {
            leases: {},
            topology: {
                "fuchsia.inspect.Graph": {
                    topology: {
                        broker.get_unsatisfiable_element_id().to_string() => {
                            meta: {
                                name: broker.get_unsatisfiable_element_name(),
                                valid_levels: broker.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}
                        },
                        unobtanium.to_string() => {
                            meta: {
                                name: "Unobtainium",
                                valid_levels: v01.clone(),
                                current_level: OFF.level as u64,
                                required_level: OFF.level as u64,
                                leases: {}
                            },
                            relationships: {},
                        },
                    },
                },
                stats: contains {},
                events: {
                    "0": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *broker.get_unsatisfiable_element_id(),
                            current_level: "unset",
                            required_level: "unset",
                        }
                    },
                    "1": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *unobtanium,
                            current_level: OFF.level as u64,
                            required_level: OFF.level as u64,
                        }
                    },
                },
            }
        }});

        broker.remove_element(&unobtanium);
        assert_eq!(broker.element_exists(unobtanium), false);
        assert_data_tree!(inspect, root: {
        test: {
            leases: {},
            topology: {
                events: {
                    "0": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *broker.get_unsatisfiable_element_id(),
                            current_level: "unset",
                            required_level: "unset",
                        }
                    },
                    "1": {
                        "@time": AnyProperty,
                        add_element: {
                            element_id: *unobtanium,
                            current_level: OFF.level as u64,
                            required_level: OFF.level as u64,
                        }
                    },
                    "2": {
                        "@time": AnyProperty,
                        rm_element: {
                            element_id: *unobtanium,
                            name: "Unobtainium",
                            valid_levels: v01.clone(),
                            current_level: OFF.level as u64,
                            required_level: OFF.level as u64,
                        }
                    },
                },
                stats: contains {},
                "fuchsia.inspect.Graph": {
                    topology: {
                        broker.get_unsatisfiable_element_id().to_string() => {
                            meta: {
                                name: broker.get_unsatisfiable_element_name(),
                                valid_levels: broker.get_unsatisfiable_element_levels(),
                                required_level: "unset",
                                current_level: "unset",
                                leases: {}
                            },
                            relationships: {}
                        },
                    },
                }
            }}});
    }

    struct BrokerStatusMatcher {
        lease: LeaseMatcher,
        required_level: RequiredLevelMatcher,
    }

    impl BrokerStatusMatcher {
        fn new() -> Self {
            Self { lease: LeaseMatcher::new(), required_level: RequiredLevelMatcher::new() }
        }

        #[track_caller]
        fn assert_matches(&self, broker: &Broker) {
            self.lease.assert_matches(broker);
            self.required_level.assert_matches(broker);
        }
    }

    struct RequiredLevelMatcher {
        elements: HashMap<ElementID, IndexedPowerLevel>,
    }

    impl RequiredLevelMatcher {
        fn new() -> Self {
            Self { elements: HashMap::new() }
        }

        fn update(&mut self, id: ElementID, required_level: IndexedPowerLevel) {
            self.elements.insert(id, required_level);
        }

        fn remove(&mut self, id: ElementID) {
            self.elements.remove(&id);
        }

        #[track_caller]
        fn assert_matches(&self, broker: &Broker) {
            for (id, expected) in &self.elements {
                let rl = broker.get_required_level(id).unwrap();
                assert_eq!(rl, *expected, "get_required_level({id}) = {rl}, expected = {expected}");
            }
        }
    }

    struct LeaseMatcher {
        leases: HashMap<LeaseID, LeaseStatus>,
    }

    impl LeaseMatcher {
        fn new() -> Self {
            Self { leases: HashMap::new() }
        }

        fn update(&mut self, id: LeaseID, status: LeaseStatus) {
            self.leases.insert(id, status);
        }

        fn remove(&mut self, id: LeaseID) {
            self.leases.remove(&id);
        }

        #[track_caller]
        fn assert_matches(&self, broker: &Broker) {
            for (id, expected_status) in &self.leases {
                let status = broker.get_lease_status(*id).expect(
                    "No lease exists with id ({id}), forgot to remove it from the matcher?",
                );
                assert_eq!(
                    status, *expected_status,
                    "get_lease_status({id}) = {status:?}, expected = {expected_status:?}"
                );
            }
        }
    }

    #[fuchsia::test]
    fn test_broker_adjust_lease_counter() {
        // Create a broker that has nothing since the test is just about the counter
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let a = broker
            .add_element("a", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");

        let b = broker
            .add_element("b", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");

        assert_eq!(broker.adjust_lease_counter(a, 5, 1), 1);
        // a:5: 1
        assert_eq!(broker.adjust_lease_counter(a, 5, 1), 2);
        // a:5: 2
        assert_eq!(broker.adjust_lease_counter(b, 6, 1), 1);
        // a:5: 2 b:6:1
        assert_eq!(broker.adjust_lease_counter(a, 15, 1), 1);
        // a:5: 2 a:15:1 b:6:1

        assert_eq!(broker.adjust_lease_counter(a, 5, 1), 3);
        // a:5: 3 a:15:1 b:6:1
        assert_eq!(broker.adjust_lease_counter(a, 5, -1), 2);
        // a:5: 2 a:15:1 b:6:1
        assert_eq!(broker.adjust_lease_counter(b, 6, -1), 0);
        // a:5: 2 a:15:1 b:6:0
        assert_eq!(broker.adjust_lease_counter(a, 5, -1), 1);
        // a:5: 1 a:15:1 b:6:0
        assert_eq!(broker.adjust_lease_counter(a, 15, -1), 0);
        // a:5: 1 a:15:0 b:6:0
    }

    #[fuchsia::test]
    async fn test_broker_lease_direct() {
        // Create a topology of a child element with two direct dependencies.
        // P1 <= C => P2
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let parent1_token = DependencyToken::create();
        let parent1: ElementID = broker
            .add_element("P1", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent1,
                parent1_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let parent2_token = DependencyToken::create();
        let parent2: ElementID = broker
            .add_element("P2", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent2,
                parent2_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let child = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            parent1_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            parent2_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");

        let mut broker_status = BrokerStatusMatcher::new();

        // All elements should start with required level OFF.
        broker_status.required_level.update(parent1, OFF);
        broker_status.required_level.update(parent2, OFF);
        broker_status.required_level.update(child, OFF);
        broker_status.assert_matches(&broker);

        let v01: Vec<u64> = BINARY_POWER_LEVELS.iter().map(|&v| v as u64).collect();
        assert_data_tree!(inspect, root: {
            test: {
                leases: {},
                topology: {
                    events: contains {},
                    stats: contains {},
                    "fuchsia.inspect.Graph": {
                        topology: {
                            broker.get_unsatisfiable_element_id().to_string() => {
                                meta: {
                                    name: broker.get_unsatisfiable_element_name(),
                                    valid_levels: broker.get_unsatisfiable_element_levels(),
                                    required_level: "unset",
                                    current_level: "unset",
                                    leases: {}
                                },
                                relationships: {}
                            },
                            parent1.to_string() => {
                                meta: {
                                    name: "P1",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {},
                            },
                            parent2.to_string() => {
                                meta: {
                                    name: "P2",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {},
                            },
                            child.to_string() => {
                                meta: {
                                    name: "C",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {
                                    parent1.to_string() => {
                                        edge_id: AnyProperty,
                                        meta: {
                                            "1": {
                                                required_level: 1u64,
                                            }
                                        },
                                    },
                                    parent2.to_string() => {
                                        edge_id: AnyProperty,
                                        meta: {
                                            "1": {
                                                required_level: 1u64,
                                            }
                                        },
                                    },
                                },
                            },
                        },
                    },
        }}});

        // Acquiring the lease should result in two direct claims.
        // P1's required level should become ON.
        // P2's required level should become ON.
        // The lease should be pending, as C isn't ON yet.
        let lease = broker.acquire_lease(child, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(parent1, ON);
        broker_status.required_level.update(parent2, ON);
        broker_status.lease.update(lease.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        assert_eq!(broker.adjust_lease_counter(child, ON.level, 0), 1);

        // Update P1's current level to ON.
        // C's required level should not change, as it also requires P2.
        broker.update_current_level(parent1, ON);
        broker_status.assert_matches(&broker);

        // Update P2's current level to ON.
        // C's required level should become ON, as both P1 and P2 are satisfied.
        broker.update_current_level(parent2, ON);
        broker_status.required_level.update(child, ON);
        broker_status.assert_matches(&broker);

        // Update C's current level to ON.
        // The lease should now be satisfied.
        broker.update_current_level(child, ON);
        broker_status.lease.update(lease.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        assert_data_tree!(inspect, root: {
            test: {
                leases: {
                    lease.id.to_string() => "Satisfied",
                },
                topology: {
                    events: contains {},
                    stats: contains {},
                    "fuchsia.inspect.Graph": {
                        topology: {
                            broker.get_unsatisfiable_element_id().to_string() => {
                                meta: {
                                    name: broker.get_unsatisfiable_element_name(),
                                    valid_levels: broker.get_unsatisfiable_element_levels(),
                                    required_level: "unset",
                                    current_level: "unset",
                                    leases: {},
                                },
                                relationships: {}
                            },
                            parent1.to_string() => {
                                meta: {
                                    name: "P1",
                                    valid_levels: v01.clone(),
                                    current_level: ON.level as u64,
                                    required_level: ON.level as u64,
                                    leases: {},
                                },
                                relationships: {},
                            },
                            parent2.to_string() => {
                                meta: {
                                    name: "P2",
                                    valid_levels: v01.clone(),
                                    current_level: ON.level as u64,
                                    required_level: ON.level as u64,
                                    leases: {},
                                },
                                relationships: {},
                            },
                            child.to_string() => {
                                meta: {
                                    name: "C",
                                    valid_levels: v01.clone(),
                                    current_level: ON.level as u64,
                                    required_level: ON.level as u64,
                                    leases: {
                                        format!("{}", lease.id) => {
                                            level: 1u64,
                                            status: 2u64,
                                        }
                                    }
                                },
                                relationships: {
                                    parent1.to_string() => {
                                        edge_id: AnyProperty,
                                        meta: {
                                            "1": {
                                                required_level: 1u64,
                                            }
                                        }
                                    },
                                    parent2.to_string() => {
                                        edge_id: AnyProperty,
                                        meta: {
                                            "1": {
                                                required_level: 1u64,
                                            }
                                        }
                                    },
                                },
                            },
                        },
                    },
        }}});

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    create_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        level: 1u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 1u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 2u64,
                    },
                },
            ]
        );

        // Dropping the lease should cause both claims to be dropped.
        // C's required level should become OFF.
        // P1 and P2's required level should remain ON, as C depends on them.
        broker.drop_lease(lease.id).expect("drop failed");
        broker_status.required_level.update(child, OFF);
        broker_status.lease.remove(lease.id);
        broker_status.assert_matches(&broker);

        assert_eq!(broker.adjust_lease_counter(child, ON.level, 0), 0);

        // Drop C's level to OFF.
        // P1's required level should become OFF, as no lease requires it.
        // P2's required level should become OFF, as no lease requires it.
        broker.update_current_level(child, OFF);
        broker_status.required_level.update(parent1, OFF);
        broker_status.required_level.update(parent2, OFF);
        broker_status.assert_matches(&broker);

        // Try dropping the lease one more time, which should result in an error.
        let extra_drop = broker.drop_lease(lease.id);
        assert!(extra_drop.is_err());

        assert_lease_cleaned_up(&broker.catalog, lease.id);

        // Because ElementIDs are randomly generated, these will be processed in order of ID hash
        // value. We don't actually care about the order for correctness outside of
        // assert_events_recordered_in_order.
        let (first_parent, second_parent) =
            if parent1 < parent2 { (parent1, parent2) } else { (parent2, parent1) };

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    create_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        level: 1u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 1u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 2u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 3u64,
                    },
                },
                {
                    update_level: {
                        element_id: *child,
                        required_level: 0u64,
                    },
                },
                {
                    update_level: {
                        element_id: *child,
                        current_level: 0u64,
                    },
                },
                {
                    update_level: {
                        element_id: *first_parent,
                        required_level: 0u64,
                    },
                },
                {
                    update_level: {
                        element_id: *second_parent,
                        required_level: 0u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        status: 4u64,
                    },
                },
                {
                    rm_lease: {
                        element_id: *child,
                        lease_id: *lease.id,
                        level: 1u64,
                    },
                },
            ]
        );

        broker.remove_element(&child);
        assert_element_cleaned_up(&broker, child);
        broker.remove_element(&parent2);
        assert_element_cleaned_up(&broker, parent2);
        broker.remove_element(&parent1);
        assert_element_cleaned_up(&broker, parent1);
    }

    #[fuchsia::test]
    fn test_broker_lease_transitive() {
        // Create a topology of a child element with two chained transitive
        // dependencies.
        // C => P => GP
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let grandparent_token = DependencyToken::create();
        let grandparent: ElementID = broker
            .add_element("GP", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                grandparent,
                grandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let parent_token = DependencyToken::create();
        let parent: ElementID = broker
            .add_element("P", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent,
                parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let child = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let dep = fpb::LevelDependency {
            dependent_level: Some(ON.level),
            requires_token: Some(
                grandparent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
            ),
            requires_level_by_preference: Some(vec![ON.level]),
            ..Default::default()
        };
        broker.add_dependency(parent, dep, &mut EagerInspectWriter).expect("add_dependency failed");
        let mut broker_status = BrokerStatusMatcher::new();

        // All elements should start with required level OFF.
        broker_status.required_level.update(parent, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.required_level.update(child, OFF);
        broker_status.assert_matches(&broker);

        // Lease C, which will result in two claims, one for P as the direct
        // dependency and GP as the transitive dependency.
        // P's required level should remain OFF, it depends on GP.
        // GP's required level should become ON.
        let lease = broker.acquire_lease(child, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(grandparent, ON);
        broker_status.lease.update(lease.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise GP's level to ON.
        // P's required level should become ON.
        broker.update_current_level(grandparent, ON);
        broker_status.required_level.update(parent, ON);
        broker_status.assert_matches(&broker);

        // Raise P's level to ON.
        // C's required level should become ON.
        broker.update_current_level(parent, ON);
        broker_status.required_level.update(child, ON);
        broker_status.assert_matches(&broker);

        // Raise C's level to ON.
        // Lease C should now be satisfied as C is ON.
        broker.update_current_level(child, ON);
        broker_status.lease.update(lease.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop Lease C.
        // C's required level should become OFF, as it's claim is dropped.
        // P's required level should remain ON, until C is OFF.
        // GP's required level should remain ON, until P is OFF.
        broker.drop_lease(lease.id).expect("drop failed");
        broker_status.required_level.update(child, OFF);
        broker_status.lease.remove(lease.id);
        broker_status.assert_matches(&broker);

        // Lower C's level to OFF.
        // P's required level should become OFF.
        broker.update_current_level(child, OFF);
        broker_status.required_level.update(parent, OFF);
        broker_status.assert_matches(&broker);

        // Lower P's required level to OFF.
        // GP's required level should become OFF.
        broker.update_current_level(parent, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.assert_matches(&broker);

        assert_lease_cleaned_up(&broker.catalog, lease.id);
    }

    #[fuchsia::test]
    fn test_broker_transitive_dependency_removed_element() {
        // Create a topology of a child element with three chained transitive
        // dependencies: C => P => GP => GGP.
        // If P is removed while C is leased, the claims on GP and GGP should be dropped.
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let greatgrandparent_token = DependencyToken::create();
        let greatgrandparent: ElementID = broker
            .add_element("GGP", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                greatgrandparent,
                greatgrandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");

        let grandparent_token = DependencyToken::create();
        let grandparent: ElementID = broker
            .add_element("GP", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                grandparent,
                grandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");

        let parent_token = DependencyToken::create();
        let parent: ElementID = broker
            .add_element("P", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent,
                parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        let child = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        // P depends on GP
        let dep_p_gp = fpb::LevelDependency {
            dependent_level: Some(ON.level),
            requires_token: Some(
                grandparent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
            ),
            requires_level_by_preference: Some(vec![ON.level]),
            ..Default::default()
        };
        broker
            .add_dependency(parent, dep_p_gp, &mut EagerInspectWriter)
            .expect("add_dependency failed");

        // GP depends on GGP
        let dep_gp_ggp = fpb::LevelDependency {
            dependent_level: Some(ON.level),
            requires_token: Some(
                greatgrandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed"),
            ),
            requires_level_by_preference: Some(vec![ON.level]),
            ..Default::default()
        };
        broker
            .add_dependency(grandparent, dep_gp_ggp, &mut EagerInspectWriter)
            .expect("add_dependency failed");

        let mut broker_status = BrokerStatusMatcher::new();

        // All elements should start with required level OFF.
        broker_status.required_level.update(parent, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.required_level.update(greatgrandparent, OFF);
        broker_status.required_level.update(child, OFF);
        broker_status.assert_matches(&broker);

        // Lease C, which will result in GGP's required level becoming ON.
        let lease = broker.acquire_lease(child, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(greatgrandparent, ON);
        broker_status.lease.update(lease.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise GGP's level to ON.
        broker.update_current_level(greatgrandparent, ON);
        // GP's required level should become ON.
        broker_status.required_level.update(grandparent, ON);
        broker_status.assert_matches(&broker);

        // Raise GP's level to ON.
        broker.update_current_level(grandparent, ON);
        // P's required level should become ON.
        broker_status.required_level.update(parent, ON);
        broker_status.assert_matches(&broker);

        // Raise P's level to ON.
        broker.update_current_level(parent, ON);
        // C's required level should become ON.
        broker_status.required_level.update(child, ON);
        broker_status.assert_matches(&broker);

        // Raise C's level to ON.
        broker.update_current_level(child, ON);
        // Lease should now be satisfied.
        broker_status.lease.update(lease.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Remove P (the intermediate element).
        // This breaks the transitive chain C => P => GP => GGP.
        // The claims on GP and GGP should be dropped.
        broker.remove_element(&parent);

        // P is removed, so we shouldn't check it.
        broker_status.required_level.remove(parent);
        broker_status.required_level.update(child, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.required_level.update(greatgrandparent, OFF);
        // Lease should become Pending again because dependency chain is broken.
        broker_status.lease.update(lease.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Clean up remaining elements
        broker.remove_element(&child);
        assert_element_cleaned_up(&broker, child);
        broker.remove_element(&grandparent);
        assert_element_cleaned_up(&broker, grandparent);
        broker.remove_element(&greatgrandparent);
        assert_element_cleaned_up(&broker, greatgrandparent);
    }

    #[fuchsia::test]
    fn test_broker_lease_shared() {
        // Create a topology of two child elements with a shared
        // parent and grandparent
        // C1 \\
        //      > P => GP
        // C2 //
        // Child 1 requires Parent at 50 to support its own level of 5.
        // Parent requires Grandparent at 200 to support its own level of 50.
        // C1 => P => GP
        //  5 => 50 => 200
        // Child 2 requires Parent at 30 to support its own level of 3.
        // Parent requires Grandparent at 90 to support its own level of 30.
        // C2 =>  P => GP
        //  3 => 30 => 90
        // Grandparent has a minimum required level of 10.
        // All other elements have a minimum of 0.
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let grandparent_token = DependencyToken::create();
        let grandparent: ElementID =
            broker.add_element("GP", 10, vec![10, 90, 200], vec![]).expect("add_element failed");
        broker
            .register_dependency_token(
                grandparent,
                grandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let parent_token = DependencyToken::create();
        let parent: ElementID = broker
            .add_element(
                "P",
                0,
                vec![0, 30, 50],
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(50),
                        requires_token: Some(
                            grandparent_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![200]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(30),
                        requires_token: Some(
                            grandparent_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![90]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent,
                parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let child1 = broker
            .add_element(
                "C1",
                0,
                vec![0, 5],
                vec![fpb::LevelDependency {
                    dependent_level: Some(5),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![50]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let child2 = broker
            .add_element(
                "C2",
                0,
                vec![0, 3],
                vec![fpb::LevelDependency {
                    dependent_level: Some(3),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![30]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        // Initially, all elements should be at their default required levels.
        // Grandparent should have a default required level of 10
        // and all others should have a default required level of 0.
        let mut broker_status = BrokerStatusMatcher::new();
        broker_status.required_level.update(parent, ZERO);
        broker_status.required_level.update(grandparent, IndexedPowerLevel { level: 10, index: 0 });
        broker_status.required_level.update(child1, ZERO);
        broker_status.required_level.update(child2, ZERO);
        broker_status.assert_matches(&broker);

        // Acquire lease for Child 1. Initially, Grandparent should have
        // required level 200 and Parent should have required level 0
        // because Child 1 has a dependency on Parent and Parent has a
        // dependency on Grandparent. Grandparent has no dependencies so its
        // level should be raised first.
        let lease1 = broker
            .acquire_lease(child1, IndexedPowerLevel { level: 5, index: 1 }, zx::Koid::from_raw(1))
            .expect("acquire failed");
        broker_status
            .required_level
            .update(grandparent, IndexedPowerLevel { level: 200, index: 2 });
        broker_status.lease.update(lease1.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise Grandparent's current level to 200. Now Parent claim should
        // be enforced, because its dependency on Grandparent is unblocked
        // raising its required level to 50.
        broker.update_current_level(grandparent, IndexedPowerLevel { level: 200, index: 2 });
        broker_status.required_level.update(parent, IndexedPowerLevel { level: 50, index: 2 });
        broker_status.assert_matches(&broker);

        // Update Parent's current level to 50.
        // Parent and Grandparent should have required levels of 50 and 200.
        broker.update_current_level(parent, IndexedPowerLevel { level: 50, index: 2 });
        broker_status.required_level.update(child1, IndexedPowerLevel { level: 5, index: 1 });
        broker_status.assert_matches(&broker);

        // Update Child 1's current level to 5.
        // Lease Child 1's is now satisfied.
        broker.update_current_level(child1, IndexedPowerLevel { level: 5, index: 1 });
        broker_status.lease.update(lease1.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Acquire a lease for Child 2. Though Child 2 has nominal
        // requirements of Parent at 30 and Grandparent at 100, they are
        // superseded by Child 1's requirements of 50 and 200.
        let lease2 = broker
            .acquire_lease(child2, IndexedPowerLevel { level: 3, index: 1 }, zx::Koid::from_raw(2))
            .expect("acquire failed");
        broker_status.required_level.update(child2, IndexedPowerLevel { level: 3, index: 1 });
        broker_status.lease.update(lease2.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Update Child 2's current level to 3.
        // Lease Child 2's is now satisfied.
        broker.update_current_level(child2, IndexedPowerLevel { level: 3, index: 1 });
        broker_status.lease.update(lease2.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop lease for Child 1.
        // Child's required level should be 0.
        broker.drop_lease(lease1.id).expect("drop failed");
        broker_status.required_level.update(child1, ZERO);
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // Drop Child 1's level to 0.
        // Parent's required level should immediately drop to 30.
        // Grandparent's required level will remain at 200 for now.
        broker.update_current_level(child1, ZERO);
        broker_status.required_level.update(parent, IndexedPowerLevel { level: 30, index: 1 });
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // Lower Parent's current level to 30. Now Grandparent's required level
        // should drop to 90.
        broker.update_current_level(parent, IndexedPowerLevel { level: 30, index: 1 });
        broker_status.required_level.update(grandparent, IndexedPowerLevel { level: 90, index: 1 });
        broker_status.assert_matches(&broker);

        // All claims for Lease 1 should now be cleaned up,
        // even though Lease 2 is still active.
        assert_lease_cleaned_up(&broker.catalog, lease1.id);

        // Drop lease for Child 2.
        // Child 2's required level should drop to 0.
        broker.drop_lease(lease2.id).expect("drop failed");
        broker_status.required_level.update(child2, ZERO);
        broker_status.lease.remove(lease2.id);
        broker_status.assert_matches(&broker);

        // Lower Child 2's current level to 0.
        // Parent should have required level 0.
        // Grandparent's required level should remain 90.
        broker.update_current_level(child2, ZERO);
        broker_status.required_level.update(parent, ZERO);
        broker_status.assert_matches(&broker);

        // Lower GrandParent's current level to 90.
        broker.update_current_level(grandparent, IndexedPowerLevel { level: 90, index: 1 });
        broker_status.assert_matches(&broker);

        // Lower Parent's current level to 0. Grandparent claim should now be
        // dropped and have its default required level of 10.
        broker.update_current_level(parent, ZERO);
        broker_status.required_level.update(grandparent, IndexedPowerLevel { level: 10, index: 0 });
        broker_status.assert_matches(&broker);
        assert_lease_cleaned_up(&broker.catalog, lease2.id);
    }

    #[fuchsia::test]
    fn test_broker_lease_redundant_claims() {
        // Create a topology of two child elements with a shared
        // parent and grandparent
        // C1 \\
        //      > P => GP
        // C2 //
        // Child 1 ON requires Parent ON.
        // Parent ON requires Grandparent ON.
        // C1 => P => GP
        // Child 2 ON requires Parent ON.
        // Parent ON requires Grandparent ON.
        // C2 =>  P => GP
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let grandparent_token = DependencyToken::create();
        let grandparent: ElementID = broker
            .add_element("GP", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                grandparent,
                grandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let parent_token = DependencyToken::create();
        let parent: ElementID = broker
            .add_element(
                "P",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        grandparent_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent,
                parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let child1 = broker
            .add_element(
                "C1",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let child2 = broker
            .add_element(
                "C2",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        // Initially, all elements should be OFF.
        let mut broker_status = BrokerStatusMatcher::new();
        broker_status.required_level.update(parent, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.required_level.update(child1, OFF);
        broker_status.required_level.update(child2, OFF);
        broker_status.assert_matches(&broker);

        // Acquire lease for Child 1. Initially, Grandparent
        // should have required level ON because Child 1 has a dependency
        // on Parent and Parent has a dependency on Grandparent. Grandparent
        // has no dependencies so its level should be raised first.
        let lease1 =
            broker.acquire_lease(child1, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(grandparent, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise Grandparent's current level to ON. Now Parent claim should
        // be enforced, because its dependency on Grandparent is unblocked
        // raising its required level to ON.
        broker.update_current_level(grandparent, ON);
        broker_status.required_level.update(parent, ON);
        broker_status.assert_matches(&broker);

        // Update Parent's current level to ON.
        // Child 1's required level should become ON.
        broker.update_current_level(parent, ON);
        broker_status.required_level.update(child1, ON);
        broker_status.assert_matches(&broker);

        // Update Child 1's current level to ON.
        // Lease Child 1's is now satisfied.
        broker.update_current_level(child1, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Acquire a lease for Child 2. Child 2 requires Parent and
        // Grandparent ON, which is already met by Child 1's requirements.
        let lease2 =
            broker.acquire_lease(child2, ON, zx::Koid::from_raw(2)).expect("acquire failed");
        broker_status.required_level.update(child2, ON);
        broker_status.lease.update(lease2.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Update Child 2's current level to ON.
        // Lease Child 2's is now satisfied.
        broker.update_current_level(child2, ON);
        broker_status.lease.update(lease2.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop lease for Child 1.
        // Child's required level should be OFF.
        broker.drop_lease(lease1.id).expect("drop failed");
        broker_status.required_level.update(child1, OFF);
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // Drop Child 1's level to OFF.
        // Parent's required level should not be affected.
        // Grandparent's required level should not be affected.
        broker.update_current_level(child1, OFF);
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // All claims for Lease 1 should now be cleaned up,
        // even though Lease 2 is still active.
        assert_lease_cleaned_up(&broker.catalog, lease1.id);

        // Drop lease for Child 2.
        // Child 2's required level should drop to OFF.
        broker.drop_lease(lease2.id).expect("drop failed");
        broker_status.required_level.update(child2, OFF);
        broker_status.lease.remove(lease2.id);
        broker_status.assert_matches(&broker);

        // Lower Child 2's current level to OFF.
        // Parent should have required level OFF.
        // Grandparent's required level should remain ON.
        broker.update_current_level(child2, OFF);
        broker_status.required_level.update(parent, OFF);
        broker_status.assert_matches(&broker);

        // Lower Parent's current level to OFF. Grandparent claim should now be
        // dropped and have its default required level of OFF.
        broker.update_current_level(parent, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.assert_matches(&broker);
        assert_lease_cleaned_up(&broker.catalog, lease2.id);
    }

    #[fuchsia::test]
    fn test_broker_lease_redundant_claims_shared_downstream() {
        // Create a topology of two pairs of chained elements with
        // a shared downstream dependency
        // A => B => C
        //           C <= D <= E
        // A ON requires B ON.
        // B ON requires C ON.
        // E ON requires D ON.
        // D ON requires C ON.
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let element_c_token = DependencyToken::create();
        let element_c: ElementID = broker
            .add_element("C", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_c,
                element_c_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let element_b_token = DependencyToken::create();
        let element_b: ElementID = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        element_c_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_b,
                element_b_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let element_a = broker
            .add_element(
                "A",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        element_b_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_d = broker
            .add_element(
                "D",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        element_c_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_d_token = DependencyToken::create();
        broker
            .register_dependency_token(
                element_d,
                element_d_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let element_e = broker
            .add_element(
                "E",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        element_d_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        log::warn!("a: {element_a}\nb: {element_b}\nc:{element_c}\nd:{element_d}\ne:{element_e}");

        // Initially, all elements should be OFF.
        let mut broker_status = BrokerStatusMatcher::new();
        broker_status.required_level.update(element_a, OFF);
        broker_status.required_level.update(element_b, OFF);
        broker_status.required_level.update(element_c, OFF);
        broker_status.required_level.update(element_d, OFF);
        broker_status.required_level.update(element_e, OFF);
        broker_status.assert_matches(&broker);

        // Acquire lease for E. C's required level should become on as a result.
        let lease1 =
            broker.acquire_lease(element_e, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(element_c, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise C's current level to ON. Now D should have its required level on.
        broker.update_current_level(element_c, ON);
        broker_status.required_level.update(element_d, ON);
        broker_status.assert_matches(&broker);

        // Update D's current level to ON. Now E should have its required level on.
        broker.update_current_level(element_d, ON);
        broker_status.required_level.update(element_e, ON);
        broker_status.assert_matches(&broker);

        // Raise E's current level to ON. Now the lease should be satisfied.
        broker.update_current_level(element_e, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Acquire a lease for A. A requires B, which is currently OFF and
        // C, which is already ON.
        let lease2 =
            broker.acquire_lease(element_a, ON, zx::Koid::from_raw(2)).expect("acquire failed");
        broker_status.required_level.update(element_b, ON);
        broker_status.lease.update(lease2.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Update B's current level to ON.
        // A's required level should become ON.
        broker.update_current_level(element_b, ON);
        broker_status.required_level.update(element_a, ON);
        broker_status.assert_matches(&broker);

        // Update A's current level to ON.
        // The lease on A is now satisfied.
        broker.update_current_level(element_a, ON);
        broker_status.lease.update(lease2.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop lease for A.
        // A's required level should become OFF.
        broker.drop_lease(lease2.id).expect("drop failed");
        broker_status.required_level.update(element_a, OFF);
        broker_status.lease.remove(lease2.id);
        broker_status.assert_matches(&broker);

        // Drop lease for E.
        // E's required level should become OFF.
        broker.drop_lease(lease1.id).expect("drop failed");
        broker_status.required_level.update(element_e, OFF);
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // Lower E's level to OFF.
        // D's required level should become OFF.
        broker.update_current_level(element_e, OFF);
        broker_status.required_level.update(element_d, OFF);
        broker_status.assert_matches(&broker);

        // Lower A's level to OFF.
        // B's required level should become OFF.
        broker.update_current_level(element_a, OFF);
        broker_status.required_level.update(element_b, OFF);
        broker_status.assert_matches(&broker);

        // Lower D's level to OFF.
        // No required levels should change.
        broker.update_current_level(element_d, OFF);
        broker_status.assert_matches(&broker);

        // Lower B's level to OFF.
        // C's required level should now become OFF.
        broker.update_current_level(element_b, OFF);
        broker_status.required_level.update(element_c, OFF);
        broker_status.assert_matches(&broker);

        // Lower C's level to OFF.
        broker.update_current_level(element_b, OFF);
        broker_status.assert_matches(&broker);

        // All claims for both leases should now be cleaned up.
        assert_lease_cleaned_up(&broker.catalog, lease1.id);
        assert_lease_cleaned_up(&broker.catalog, lease2.id);
    }

    #[fuchsia::test]
    fn test_broker_lease_shared_required_element() {
        // Create a topology of one child element with two distinct
        // parents and shared grandparent
        //     > P1
        //   //     \\
        // C           > GP
        //   \\      //
        //     > P2
        //
        // Child ON requires Parent 1 ON and Parent 2 ON.
        // Parent 1 ON requires Grandparent ON.
        // Parent 2 ON requires Grandparent ON.
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let grandparent_token = DependencyToken::create();
        let grandparent: ElementID = broker
            .add_element("GP", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                grandparent,
                grandparent_token
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("dup failed")
                    .into(),
            )
            .expect("register_dependency_token failed");
        let parent1_token = DependencyToken::create();
        let parent1: ElementID = broker
            .add_element(
                "P",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        grandparent_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent1,
                parent1_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let parent2_token = DependencyToken::create();
        let parent2: ElementID = broker
            .add_element(
                "P",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        grandparent_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                parent2,
                parent2_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let child = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            parent1_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            parent2_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");

        // Initially, all elements should be OFF.
        let mut broker_status = BrokerStatusMatcher::new();
        broker_status.required_level.update(grandparent, OFF);
        broker_status.required_level.update(parent1, OFF);
        broker_status.required_level.update(parent2, OFF);
        broker_status.required_level.update(child, OFF);
        broker_status.assert_matches(&broker);

        // Acquire lease for Child.
        let lease1 =
            broker.acquire_lease(child, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        broker_status.required_level.update(grandparent, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Raise Grandparent's current level to ON. Now both Parent claims should
        // be enforced, because its dependency on Grandparent is unblocked
        // raising its required level to ON.
        broker.update_current_level(grandparent, ON);
        broker_status.required_level.update(parent1, ON);
        broker_status.required_level.update(parent2, ON);
        broker_status.assert_matches(&broker);

        // Update Parent 1's current level to ON.
        // Child's required level should not increase because Parent 2 is not yet ON.
        broker.update_current_level(parent1, ON);
        broker_status.assert_matches(&broker);

        // Update Parent 2's current level to ON.
        // Child's required level should become ON.
        broker.update_current_level(parent2, ON);
        broker_status.required_level.update(child, ON);
        broker_status.assert_matches(&broker);

        // Update Child's current level to ON.
        // Lease on Child is now satisfied.
        broker.update_current_level(child, ON);
        broker_status.lease.update(lease1.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop lease for Child.
        // Child's required level should be OFF.
        broker.drop_lease(lease1.id).expect("drop failed");
        broker_status.required_level.update(child, OFF);
        broker_status.lease.remove(lease1.id);
        broker_status.assert_matches(&broker);

        // Lower Child's level to OFF.
        // Parent 1's required level should become OFF.
        // Parent 2's required level should become OFF.
        // Grandparent's required level should not be affected.
        broker.update_current_level(child, OFF);
        broker_status.required_level.update(parent1, OFF);
        broker_status.required_level.update(parent2, OFF);
        broker_status.assert_matches(&broker);

        // Lower Parent 1's level to OFF.
        // Grandparent's required level should not be affected because Parent 2 is still ON.
        broker.update_current_level(parent1, OFF);
        broker_status.assert_matches(&broker);

        // Lower Parent 2's level to OFF.
        // Grandparent's required level should become OFF.
        broker.update_current_level(parent2, OFF);
        broker_status.required_level.update(grandparent, OFF);
        broker_status.assert_matches(&broker);
        assert_lease_cleaned_up(&broker.catalog, lease1.id);
    }

    #[fuchsia::test]
    async fn test_lease_cumulative_implicit_dependency() {
        // Tests that cumulative implicit dependencies are properly resolved when a lease is
        // acquired. Verifies a simple case of dependencies only.
        //
        // A[1] has an dependency on B[1].
        // A[2] has an dependency on C[1].
        //
        // A[2] has an implicit, dependency on B[1].
        //
        //  A     B     C
        //  1 ==> 1
        //  2 ========> 1
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let v012_u8: Vec<u8> = vec![0, 1, 2];

        let token_b = DependencyToken::create();
        let token_c = DependencyToken::create();
        let element_b =
            broker.add_element("B", 0, v012_u8.clone(), vec![]).expect("add_element failed");
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_c =
            broker.add_element("C", 0, v012_u8.clone(), vec![]).expect("add_element failed");
        broker
            .register_dependency_token(
                element_c,
                token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_a = broker
            .add_element(
                "A",
                0,
                v012_u8.clone(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(1),
                        requires_token: Some(
                            token_b
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed")
                                .into(),
                        ),
                        requires_level_by_preference: Some(vec![1]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(2),
                        requires_token: Some(
                            token_c
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed")
                                .into(),
                        ),
                        requires_level_by_preference: Some(vec![1]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");

        let mut broker_status = BrokerStatusMatcher::new();

        // Initial required levels for all elements should be 0.
        // Set all current levels to 0.
        broker.update_current_level(element_a, ZERO);
        broker.update_current_level(element_b, ZERO);
        broker.update_current_level(element_c, ZERO);
        broker_status.required_level.update(element_a, ZERO);
        broker_status.required_level.update(element_b, ZERO);
        broker_status.required_level.update(element_c, ZERO);
        broker_status.assert_matches(&broker);

        // Lease A[2].
        //
        // A has two dependencies, B[1] and C[1].
        //
        // A's required level should not change.
        // B and C's required level should be 1.
        //
        // A's lease is pending.
        let lease_a =
            broker.acquire_lease(element_a, TWO, zx::Koid::from_raw(1)).expect("acquire failed");
        let lease_a_id = lease_a.id;
        broker_status.required_level.update(element_b, ONE);
        broker_status.required_level.update(element_c, ONE);
        broker_status.assert_matches(&broker);
        assert_eq!(broker.get_lease_status(lease_a.id), Some(LeaseStatus::Pending));

        // Update B, C's current level to 1.
        //
        // A's current level should now be 2.
        // B and C's current level should not change.
        //
        // A's lease should be satisfied.
        broker.update_current_level(element_b, ONE);
        broker.update_current_level(element_c, ONE);
        broker_status.required_level.update(element_a, TWO);
        broker_status.assert_matches(&broker);

        // Update A's current level to 2.
        //
        // A's current level should now be 2.
        // B and C's current level should not change.
        //
        // A's lease should be satisfied.
        broker.update_current_level(element_a, TWO);
        broker_status.lease.update(lease_a.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop Lease A.
        //
        // A's required level should become 0, as it is no longer leased.
        //
        // Lease A should be pending.
        broker.drop_lease(lease_a.id).expect("drop_lease failed");
        broker_status.required_level.update(element_a, ZERO);
        broker_status.lease.remove(lease_a.id);
        broker_status.assert_matches(&broker);

        // Update A's current level to 0.
        //
        // B's required level should become 0, as A no longer needs it.
        // C's required level should become 0, as A no longer needs it.
        broker.update_current_level(element_a, ZERO);
        broker_status.required_level.update(element_b, ZERO);
        broker_status.required_level.update(element_c, ZERO);
        broker_status.assert_matches(&broker);

        // All leases should be cleaned up.
        assert_lease_cleaned_up(&broker.catalog, lease_a_id);
    }

    #[fuchsia::test]
    fn test_removing_element_permanently_prevents_lease_satisfaction() {
        // Tests that if element A depends on element B, and element B is removed, that new leases
        // on element A will never be satisfied.
        //
        // B has a dependency on A.
        // C has a dependency on A.
        //  A     B     C
        // ON <= ON
        // ON <======= ON
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let token_a = DependencyToken::create();
        let element_a = broker
            .add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_a
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_c = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_a
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        let mut broker_status = BrokerStatusMatcher::new();

        // Initial required levels for all elements should be 0.
        // Set all current levels to 0.
        broker.update_current_level(element_a, ZERO);
        broker.update_current_level(element_b, ZERO);
        broker.update_current_level(element_c, ZERO);
        broker_status.required_level.update(element_a, ZERO);
        broker_status.required_level.update(element_b, ZERO);
        broker_status.required_level.update(element_c, ZERO);
        broker_status.assert_matches(&broker);

        // Remove A.
        // B & C's required level should remain OFF.
        broker.remove_element(&element_a);
        broker_status.required_level.remove(element_a);
        broker_status.assert_matches(&broker);

        // Lease B & C.
        // B & C's required level should remain OFF.
        // Both leases should be pending, as they should
        // have a new dependency on the topology unsatisfiable element.
        let lease_b =
            broker.acquire_lease(element_b, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        let lease_c =
            broker.acquire_lease(element_c, ON, zx::Koid::from_raw(2)).expect("acquire failed");
        broker.update_current_level(element_a, ON);
        broker_status.lease.update(lease_b.id, LeaseStatus::Pending);
        broker_status.lease.update(lease_c.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        broker.drop_lease(lease_b.id).expect("drop_lease failed");
        broker.drop_lease(lease_c.id).expect("drop_lease failed");
        broker_status.lease.remove(lease_b.id);
        broker_status.lease.remove(lease_c.id);
        broker_status.assert_matches(&broker);

        // Leases should be cleaned up.
        assert_lease_cleaned_up(&broker.catalog, lease_b.id);
        assert_lease_cleaned_up(&broker.catalog, lease_c.id);
    }

    #[fuchsia::test]
    fn test_required_level() {
        // Create a topology of one element.
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let element =
            broker.add_element("E", 0, vec![0, 1, 2], vec![]).expect("add_element failed");
        let mut broker_status = BrokerStatusMatcher::new();

        // Initial required level should be 0.
        broker_status.required_level.update(element, ZERO);
        broker_status.assert_matches(&broker);

        // Acquire lease for level 1.
        let lease =
            broker.acquire_lease(element, ONE, zx::Koid::from_raw(1)).expect("acquire failed");
        // Required level should become 1.
        broker_status.required_level.update(element, ONE);
        broker_status.assert_matches(&broker);

        // Drop lease.
        broker.drop_lease(lease.id).expect("drop failed");
        // Required level should stay 1, to preserve orderly transition.
        broker_status.assert_matches(&broker);

        // Update current level to 1 to finish transition.
        broker.update_current_level(element, ONE);
        // Required level should become 0.
        broker_status.required_level.update(element, ZERO);
        broker_status.assert_matches(&broker);

        // Acquire and drop a level 2 lease.
        let lease =
            broker.acquire_lease(element, TWO, zx::Koid::from_raw(2)).expect("acquire failed");
        broker.drop_lease(lease.id).expect("drop failed");
        // Required level should stay 0, to preserve orderly transition.
        broker_status.assert_matches(&broker);

        // Update current level to 0 to finish transition.
        broker.update_current_level(element, ZERO);
        // Required level should remain 0 as the lease has been dropped.
        broker_status.assert_matches(&broker);

        // Acquire lease for level 1 and check required level.
        let lease =
            broker.acquire_lease(element, ONE, zx::Koid::from_raw(3)).expect("acquire failed");
        broker_status.required_level.update(element, ONE);
        broker_status.assert_matches(&broker);

        // Drop lease and check required level.
        broker.drop_lease(lease.id).expect("drop failed");
        // Required level should stay 1, to preserve orderly transition.
        broker_status.assert_matches(&broker);

        // Update current level to 1 to finish transition.
        broker.update_current_level(element, ONE);
        // Required level should become 0.
        broker_status.required_level.update(element, ZERO);
        broker_status.assert_matches(&broker);
    }

    #[fuchsia::test]
    async fn test_add_element_dependency_list_of_levels() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));
        let token_mithril = DependencyToken::create();
        let mithril = broker
            .add_element("Mithril", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                mithril,
                token_mithril.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let v01: Vec<u64> = BINARY_POWER_LEVELS.iter().map(|&v| v as u64).collect();

        // Add an element with a dependency with a list of requires_level_by_preference.
        // The dependency should be taken on ON, because the other levels do not
        // exist.
        let silver = broker
            .add_element(
                "Silver",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_mithril
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![40, 30, ON.level, 20]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        assert_data_tree!(inspect, root: {
            test: {
                leases: {},
                topology: {
                    events: {
                        "0": {
                            "@time": AnyProperty,
                            add_element: {
                                current_level: "unset",
                                required_level: "unset",
                                element_id: *broker.get_unsatisfiable_element_id(),
                            }
                        },
                        "1": {
                            "@time": AnyProperty,
                            add_element: {
                                current_level: OFF.level as u64,
                                required_level: OFF.level as u64,
                                element_id: *mithril,
                            }
                        },
                        "2": {
                            "@time": AnyProperty,
                            add_element: {
                                current_level: OFF.level as u64,
                                required_level: OFF.level as u64,
                                element_id: *silver,
                                dependencies: {
                                    "0": {
                                        dependent_level: ON.level as u64,
                                        required_element: *mithril,
                                        required_level: ON.level as u64,
                                    }
                                }
                            }
                        },
                    },
                    stats: contains {},
                    "fuchsia.inspect.Graph": {
                        topology: {
                            broker.get_unsatisfiable_element_id().to_string() => {
                                meta: {
                                    name: broker.get_unsatisfiable_element_name(),
                                    valid_levels: broker.get_unsatisfiable_element_levels(),
                                    required_level: "unset",
                                    current_level: "unset",
                                    leases: {}
                                },
                                relationships: {}
                            },
                            mithril.to_string() => {
                                meta: {
                                    name: "Mithril",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {},
                            },
                            silver.to_string() => {
                                meta: {
                                    name: "Silver",
                                    valid_levels: v01.clone(),
                                    current_level: OFF.level as u64,
                                    required_level: OFF.level as u64,
                                    leases: {}
                                },
                                relationships: {
                                    mithril.to_string() => {
                                        edge_id: AnyProperty,
                                        meta: {
                                            "1": {
                                                required_level: 1u64,
                                            }
                                        }
                                    },
                                },
                            },
                        },
        }}}});
    }

    #[fuchsia::test]
    async fn test_disorderly_element() {
        // Tests that when an element behaves in a disorderly fashion, the broker does
        // not drop the levels of any elements that it depends on, but immediately drops
        // the levels of any elements that depend on it. An element's level change is
        // considered disorderly if it drops its current level even though it has been
        // requested to be at a higher required level.
        //
        // X is our 'disorderly element', it has a dependency on grandparent GP1.
        // X has a dependency on grandparent GP2.
        // P1 (parent) has a dependency on X.
        // P2 (parent) has a dependency on X.
        // C1 and C2 have dependencies on P1.
        // C3 and C4 have dependencies on P2.
        //
        // C1 = P1   GP1
        // C2 //  \\ //
        //          X
        // C4 \\  // \\
        // C3 = P2   GP2
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let token_gp1 = DependencyToken::create();
        let token_gp2 = DependencyToken::create();
        let token_x = DependencyToken::create();
        let token_p1: fidl::Event = DependencyToken::create();
        let token_p2 = DependencyToken::create();

        let element_gp1 = broker
            .add_element("GP1", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_gp1,
                token_gp1.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_gp2 = broker
            .add_element("GP2", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_gp2,
                token_gp2.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_x = broker
            .add_element(
                "X",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            token_gp1
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed")
                                .into(),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(ON.level),
                        requires_token: Some(
                            token_gp2
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed")
                                .into(),
                        ),
                        requires_level_by_preference: Some(vec![ON.level]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_x,
                token_x.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_p1 = broker
            .add_element(
                "P1",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_x
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_p1,
                token_p1.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_p2 = broker
            .add_element(
                "P2",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_x
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        broker
            .register_dependency_token(
                element_p2,
                token_p2.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");
        let element_c1 = broker
            .add_element(
                "C1",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_p1
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_c2 = broker
            .add_element(
                "C2",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_p1
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_c3 = broker
            .add_element(
                "C3",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_p2
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let element_c4 = broker
            .add_element(
                "C4",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_p2
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed")
                            .into(),
                    ),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");

        let mut broker_status = BrokerStatusMatcher::new();

        // Grab all required leases and power on all elements.
        let lease_gp2 =
            broker.acquire_lease(element_gp2, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        let lease_c1 =
            broker.acquire_lease(element_c1, ON, zx::Koid::from_raw(2)).expect("acquire failed");
        let lease_c2 =
            broker.acquire_lease(element_c2, ON, zx::Koid::from_raw(3)).expect("acquire failed");
        let lease_c3 =
            broker.acquire_lease(element_c3, ON, zx::Koid::from_raw(4)).expect("acquire failed");
        let lease_c4 =
            broker.acquire_lease(element_c4, ON, zx::Koid::from_raw(5)).expect("acquire failed");
        broker.update_current_level(element_gp1, ON);
        broker.update_current_level(element_gp2, ON);
        broker.update_current_level(element_x, ON);
        broker.update_current_level(element_p1, ON);
        broker.update_current_level(element_p2, ON);
        broker.update_current_level(element_c1, ON);
        broker.update_current_level(element_c2, ON);
        broker.update_current_level(element_c3, ON);
        broker.update_current_level(element_c4, ON);

        // At this point, all elements should have required level ON.
        broker_status.required_level.update(element_gp1, ON);
        broker_status.required_level.update(element_gp2, ON);
        broker_status.required_level.update(element_x, ON);
        broker_status.required_level.update(element_p1, ON);
        broker_status.required_level.update(element_p2, ON);
        broker_status.required_level.update(element_c1, ON);
        broker_status.required_level.update(element_c2, ON);
        broker_status.required_level.update(element_c3, ON);
        broker_status.required_level.update(element_c4, ON);
        broker_status.lease.update(lease_gp2.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c1.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c2.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c3.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c4.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Trigger disorderly drop of X.
        // All parents and grandparents of X should immediately drop to OFF.
        // X's required level should also become OFF, to
        // All leases, except for GP2, should now be pending.
        broker.update_current_level(element_x, OFF);
        broker_status.required_level.update(element_p1, OFF);
        broker_status.required_level.update(element_p2, OFF);
        broker_status.required_level.update(element_c1, OFF);
        broker_status.required_level.update(element_c2, OFF);
        broker_status.required_level.update(element_c3, OFF);
        broker_status.required_level.update(element_c4, OFF);
        broker_status.lease.update(lease_c1.id, LeaseStatus::Pending);
        broker_status.lease.update(lease_c2.id, LeaseStatus::Pending);
        broker_status.lease.update(lease_c3.id, LeaseStatus::Pending);
        broker_status.lease.update(lease_c4.id, LeaseStatus::Pending);
        broker_status.assert_matches(&broker);

        // Turn off all parent/child elements to preserve ordering.
        broker.update_current_level(element_p1, OFF);
        broker.update_current_level(element_p2, OFF);
        broker.update_current_level(element_c1, OFF);
        broker.update_current_level(element_c2, OFF);
        broker.update_current_level(element_c3, OFF);
        broker.update_current_level(element_c4, OFF);
        broker_status.assert_matches(&broker);

        // Update X to ON.
        // P1 and P2's required level should become ON.
        broker.update_current_level(element_x, ON);
        broker_status.required_level.update(element_p1, ON);
        broker_status.required_level.update(element_p2, ON);
        broker_status.assert_matches(&broker);

        // Update P1 and P2 to ON.
        broker.update_current_level(element_p1, ON);
        broker.update_current_level(element_p2, ON);
        broker_status.required_level.update(element_c1, ON);
        broker_status.required_level.update(element_c2, ON);
        broker_status.required_level.update(element_c3, ON);
        broker_status.required_level.update(element_c4, ON);
        broker_status.assert_matches(&broker);

        // Update C1, C2, C3, and C4 to ON.
        // All leases should now be satisfied.
        broker.update_current_level(element_c1, ON);
        broker.update_current_level(element_c2, ON);
        broker.update_current_level(element_c3, ON);
        broker.update_current_level(element_c4, ON);
        broker_status.lease.update(lease_c1.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c2.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c3.id, LeaseStatus::Satisfied);
        broker_status.lease.update(lease_c4.id, LeaseStatus::Satisfied);
        broker_status.assert_matches(&broker);

        // Drop all leases, power down children.
        broker.drop_lease(lease_gp2.id).expect("drop_lease failed");
        broker.drop_lease(lease_c1.id).expect("drop_lease failed");
        broker.drop_lease(lease_c2.id).expect("drop_lease failed");
        broker.drop_lease(lease_c3.id).expect("drop_lease failed");
        broker.drop_lease(lease_c4.id).expect("drop_lease failed");
        broker_status.required_level.update(element_c1, OFF);
        broker_status.required_level.update(element_c2, OFF);
        broker_status.required_level.update(element_c3, OFF);
        broker_status.required_level.update(element_c4, OFF);
        broker_status.lease.remove(lease_gp2.id);
        broker_status.lease.remove(lease_c1.id);
        broker_status.lease.remove(lease_c2.id);
        broker_status.lease.remove(lease_c3.id);
        broker_status.lease.remove(lease_c4.id);
        broker_status.assert_matches(&broker);

        // Power down all elements to remove activated claims.
        broker.update_current_level(element_c1, OFF);
        broker.update_current_level(element_c2, OFF);
        broker.update_current_level(element_c3, OFF);
        broker.update_current_level(element_c4, OFF);
        broker_status.required_level.update(element_p1, OFF);
        broker_status.required_level.update(element_p2, OFF);
        broker_status.assert_matches(&broker);

        broker.update_current_level(element_p1, OFF);
        broker.update_current_level(element_p2, OFF);
        broker_status.required_level.update(element_x, OFF);
        broker_status.assert_matches(&broker);

        broker.update_current_level(element_x, OFF);
        broker_status.required_level.update(element_gp1, OFF);
        broker_status.required_level.update(element_gp2, OFF);
        broker_status.assert_matches(&broker);

        // All leases should be cleaned up.
        assert_lease_cleaned_up(&broker.catalog, lease_gp2.id);
        assert_lease_cleaned_up(&broker.catalog, lease_c1.id);
        assert_lease_cleaned_up(&broker.catalog, lease_c2.id);
        assert_lease_cleaned_up(&broker.catalog, lease_c3.id);
        assert_lease_cleaned_up(&broker.catalog, lease_c4.id);
    }

    #[fuchsia::test]
    async fn test_direct_lease() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        // B has a dependency on A.
        // C has a dependency on D.
        // We will directly lease both B and C.
        // A <= B <= L => C => D
        let element_a =
            broker.add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![]).unwrap();
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_a.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();
        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_d =
            broker.add_element("D", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![]).unwrap();
        let token_d = DependencyToken::create();
        broker
            .register_dependency_token(
                element_d,
                token_d.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_c = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_d.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();
        let token_c = DependencyToken::create();
        broker
            .register_dependency_token(
                element_c,
                token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        // Direct Lease L depends on B and C.
        let dependencies = vec![
            fpb::LeaseDependency {
                requires_token: Some(token_b.into()),
                requires_level_by_preference: Some(vec![ON.level]),
                ..Default::default()
            },
            fpb::LeaseDependency {
                requires_token: Some(token_c.into()),
                requires_level_by_preference: Some(vec![ON.level]),
                ..Default::default()
            },
        ];

        let lease_token_koid = zx::Koid::from_raw(123);
        let lease =
            broker.acquire_direct_lease("L".to_string(), dependencies, lease_token_koid).unwrap();

        // Initially, only root elements (A and D) should have their required levels updated.
        // B and C depend on A and D respectively, so they are not yet activated.
        assert_eq!(broker.get_required_level(&element_a), Some(ON));
        assert_eq!(broker.get_required_level(&element_d), Some(ON));
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));

        // Lease should be Pending.
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Pending));

        // Now satisfy A and D.
        broker.update_current_level(element_a, ON);
        broker.update_current_level(element_d, ON);

        // Now B and C should have their required levels updated to ON.
        assert_eq!(broker.get_required_level(&element_b), Some(ON));
        assert_eq!(broker.get_required_level(&element_c), Some(ON));
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Pending));

        // Now satisfy B and C.
        broker.update_current_level(element_b, ON);
        broker.update_current_level(element_c, ON);
        // Now all dependencies are satisfied.
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        // Drop the lease.
        broker.drop_lease(lease.id).unwrap();
        // B and C's required levels should immediately drop to OFF.
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
        // A and D's required levels should remain ON.
        assert_eq!(broker.get_required_level(&element_a), Some(ON));
        assert_eq!(broker.get_required_level(&element_d), Some(ON));

        // Power down B and C.
        broker.update_current_level(element_b, OFF);
        broker.update_current_level(element_c, OFF);

        // Now A and D's required levels levels should drop to OFF.
        assert_eq!(broker.get_required_level(&element_a), Some(OFF));
        assert_eq!(broker.get_required_level(&element_d), Some(OFF));
        // B and C's required levels should remain OFF.
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
    }

    // When a direct lease is requested with no dependencies, it should be
    // rejected rather than accepted as a lease with nothing to claim.
    #[fuchsia::test]
    async fn test_direct_lease_requires_dependencies() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        // A lease with no dependencies would have no targets and no claims,
        // leaving it trivially satisfied and invisible in Inspect.
        let res = broker.acquire_direct_lease("L".to_string(), vec![], zx::Koid::from_raw(123));
        assert!(matches!(res, Err(fpb::LeaseError::InvalidArgument)));
    }

    // When a single direct lease targets two elements that share ancestors, the
    // shared dependencies should collapse into one claim at the highest level
    // required, and the lease should be recorded only under its own targets.
    #[fuchsia::test]
    async fn test_direct_lease_with_shared_ancestor() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        // B and C share the ancestors M and A, but require them at different
        // levels. A single direct lease on both B and C must collapse the
        // shared M -> A dependency into one claim at the higher level.
        //
        //   A <= M <= B <= L
        //        M <= C <= L
        let levels = vec![0, 1, 2];
        let element_a = broker
            .add_element("A", ZERO.level, levels.clone(), vec![])
            .expect("add_element failed");
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        // M at 1 requires A at 1, and M at 2 requires A at 2.
        let element_m = broker
            .add_element(
                "M",
                ZERO.level,
                levels.clone(),
                vec![
                    fpb::LevelDependency {
                        dependent_level: Some(ONE.level),
                        requires_token: Some(
                            token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                        ),
                        requires_level_by_preference: Some(vec![ONE.level]),
                        ..Default::default()
                    },
                    fpb::LevelDependency {
                        dependent_level: Some(TWO.level),
                        requires_token: Some(token_a.into()),
                        requires_level_by_preference: Some(vec![TWO.level]),
                        ..Default::default()
                    },
                ],
            )
            .expect("add_element failed");
        let token_m = DependencyToken::create();
        broker
            .register_dependency_token(
                element_m,
                token_m.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        // B at ON requires M at 1.
        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(
                        token_m.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                    ),
                    requires_level_by_preference: Some(vec![ONE.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        // C at ON requires M at 2.
        let element_c = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_m.into()),
                    requires_level_by_preference: Some(vec![TWO.level]),
                    ..Default::default()
                }],
            )
            .expect("add_element failed");
        let token_c = DependencyToken::create();
        broker
            .register_dependency_token(
                element_c,
                token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let dependencies = vec![
            fpb::LeaseDependency {
                requires_token: Some(token_b.into()),
                requires_level_by_preference: Some(vec![ON.level]),
                ..Default::default()
            },
            fpb::LeaseDependency {
                requires_token: Some(token_c.into()),
                requires_level_by_preference: Some(vec![ON.level]),
                ..Default::default()
            },
        ];
        let lease = broker
            .acquire_direct_lease("L".to_string(), dependencies, zx::Koid::from_raw(123))
            .expect("acquire_direct_lease failed");

        // Five claims, not six: the root claims on B and C, the claims on M
        // from each of B and C, and a single deduped claim on A.
        let pending = broker.catalog.pending_for_lease(lease.id);
        let enforced: Vec<Dependency> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        let all_claims: Vec<_> = pending.into_iter().chain(enforced.into_iter()).collect();
        assert_eq!(all_claims.len(), 5);
        let claims_on_a: Vec<_> =
            all_claims.iter().filter(|dep| dep.requires.element_id == element_a).collect();
        assert_eq!(claims_on_a.len(), 1);
        assert_eq!(claims_on_a[0].requires.level, TWO);

        // A is a root element, so its claim activates immediately at the
        // higher of the two levels its descendants require.
        assert_eq!(broker.get_required_level(&element_a), Some(TWO));
        assert_eq!(broker.get_required_level(&element_m), Some(ZERO));
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Pending));

        // Satisfying A raises M to the highest level required of it.
        broker.update_current_level(element_a, TWO);
        assert_eq!(broker.get_required_level(&element_m), Some(TWO));

        // Satisfying M raises both lease targets.
        broker.update_current_level(element_m, TWO);
        assert_eq!(broker.get_required_level(&element_b), Some(ON));
        assert_eq!(broker.get_required_level(&element_c), Some(ON));

        broker.update_current_level(element_b, ON);
        broker.update_current_level(element_c, ON);
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        // The lease is recorded under each of its target elements, and only
        // under those: M and A are dependencies, not targets.
        assert_data_tree!(inspect, root: contains {
            test: contains {
                topology: contains {
                    "fuchsia.inspect.Graph": contains {
                        topology: contains {
                            element_b.to_string() => contains {
                                meta: contains {
                                    leases: {
                                        format!("{}", lease.id) => {
                                            name: "L",
                                            level: ON.level as u64,
                                            status: LeaseStatus::Satisfied.into_primitive() as u64,
                                        },
                                    },
                                },
                            },
                            element_c.to_string() => contains {
                                meta: contains {
                                    leases: {
                                        format!("{}", lease.id) => {
                                            name: "L",
                                            level: ON.level as u64,
                                            status: LeaseStatus::Satisfied.into_primitive() as u64,
                                        },
                                    },
                                },
                            },
                            element_m.to_string() => contains {
                                meta: contains { leases: {} },
                            },
                            element_a.to_string() => contains {
                                meta: contains { leases: {} },
                            },
                        },
                    },
                },
            },
        });

        // Dropping the lease releases the targets immediately, then the shared
        // ancestors as each level powers down.
        broker.drop_lease(lease.id).unwrap();
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
        assert_eq!(broker.get_required_level(&element_m), Some(TWO));

        broker.update_current_level(element_b, OFF);
        broker.update_current_level(element_c, OFF);
        assert_eq!(broker.get_required_level(&element_m), Some(ZERO));

        broker.update_current_level(element_m, ZERO);
        assert_eq!(broker.get_required_level(&element_a), Some(ZERO));
        assert_lease_cleaned_up(&broker.catalog, lease.id);

        // The lease record is cleared from every target element.
        assert_data_tree!(inspect, root: contains {
            test: contains {
                topology: contains {
                    "fuchsia.inspect.Graph": contains {
                        topology: contains {
                            element_b.to_string() => contains {
                                meta: contains { leases: {} },
                            },
                            element_c.to_string() => contains {
                                meta: contains { leases: {} },
                            },
                        },
                    },
                },
            },
        });

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    rm_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        name: "L",
                        level: ON.level as u64,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
                {
                    rm_lease: {
                        element_id: *element_c,
                        lease_id: *lease.id,
                        name: "L",
                        level: ON.level as u64,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
            ]
        );
    }

    // When a direct lease names the same element more than once, those demands
    // should collapse into a single target at the highest level requested, so
    // that the element is claimed, counted, and recorded in Inspect only once.
    #[fuchsia::test]
    async fn test_direct_lease_with_repeated_element() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a =
            broker.add_element("A", ZERO.level, vec![0, 1, 2], vec![]).expect("add_element failed");
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        // The same element at the same level twice.
        let dependencies = vec![
            fpb::LeaseDependency {
                requires_token: Some(
                    token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                ),
                requires_level_by_preference: Some(vec![ONE.level]),
                ..Default::default()
            },
            fpb::LeaseDependency {
                requires_token: Some(
                    token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                ),
                requires_level_by_preference: Some(vec![ONE.level]),
                ..Default::default()
            },
        ];
        let lease = broker
            .acquire_direct_lease("L".to_string(), dependencies, zx::Koid::from_raw(123))
            .expect("acquire_direct_lease failed");

        assert_eq!(lease.targets.len(), 1);
        assert_eq!(lease.targets[0].level, ONE);
        assert_eq!(broker.adjust_lease_counter(element_a, ONE.level, 0), 1);

        // A is a root element, so its claim activates immediately.
        assert_eq!(broker.get_required_level(&element_a), Some(ONE));
        broker.update_current_level(element_a, ONE);
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        broker.drop_lease(lease.id).unwrap();
        assert_eq!(broker.adjust_lease_counter(element_a, ONE.level, 0), 0);
        assert_eq!(broker.get_required_level(&element_a), Some(ZERO));
        assert_lease_cleaned_up(&broker.catalog, lease.id);
        broker.update_current_level(element_a, ZERO);

        // The same element at two different levels. Inspect keys a lease record
        // by element, not by element level, so both demands must land on one
        // target for the record to be written once.
        let dependencies = vec![
            fpb::LeaseDependency {
                requires_token: Some(
                    token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
                ),
                requires_level_by_preference: Some(vec![ONE.level]),
                ..Default::default()
            },
            fpb::LeaseDependency {
                requires_token: Some(token_a.into()),
                requires_level_by_preference: Some(vec![TWO.level]),
                ..Default::default()
            },
        ];
        let lease = broker
            .acquire_direct_lease("L".to_string(), dependencies, zx::Koid::from_raw(456))
            .expect("acquire_direct_lease failed");

        // Collapsed to the higher of the two levels.
        assert_eq!(lease.targets.len(), 1);
        assert_eq!(lease.targets[0].level, TWO);
        assert_eq!(broker.adjust_lease_counter(element_a, TWO.level, 0), 1);
        assert_eq!(broker.adjust_lease_counter(element_a, ONE.level, 0), 0);
        assert_eq!(broker.get_required_level(&element_a), Some(TWO));
        broker.update_current_level(element_a, TWO);
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        // A single claim on A, and a single lease record under it.
        let pending = broker.catalog.pending_for_lease(lease.id);
        let enforced: Vec<Dependency> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        assert_eq!(pending.len() + enforced.len(), 1);
        assert_data_tree!(inspect, root: contains {
            test: contains {
                topology: contains {
                    "fuchsia.inspect.Graph": contains {
                        topology: contains {
                            element_a.to_string() => contains {
                                meta: contains {
                                    leases: {
                                        format!("{}", lease.id) => {
                                            name: "L",
                                            level: TWO.level as u64,
                                            status: LeaseStatus::Satisfied.into_primitive() as u64,
                                        },
                                    },
                                },
                            },
                        },
                    },
                },
            },
        });

        broker.drop_lease(lease.id).unwrap();
        assert_eq!(broker.adjust_lease_counter(element_a, TWO.level, 0), 0);
        assert_lease_cleaned_up(&broker.catalog, lease.id);
    }

    // When an element that a direct lease targets is removed, the lease and all
    // claims held beneath it should be dropped immediately and its Inspect
    // lifecycle events emitted before the target vertex is removed.
    #[fuchsia::test]
    async fn test_remove_element_that_is_direct_lease_target() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        // A <= B <= L. B is the lease target, so the lease holds a root claim
        // on B. Removing B must drop the lease along with its claims on B and A.
        let element_a =
            broker.add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![]).unwrap();
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_a.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();
        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let dependencies = vec![fpb::LeaseDependency {
            requires_token: Some(token_b.into()),
            requires_level_by_preference: Some(vec![ON.level]),
            ..Default::default()
        }];
        let lease = broker
            .acquire_direct_lease("L".to_string(), dependencies, zx::Koid::from_raw(123))
            .expect("acquire_direct_lease failed");

        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_required_level(&element_b), Some(ON));
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Pending));

        // Remove the lease target out from under the lease.
        broker.remove_element(&element_b);
        assert_eq!(broker.element_exists(element_b), false);

        // The lease and all of its claims are dropped immediately, leaving A no longer required.
        assert_eq!(broker.get_required_level(&element_a), Some(OFF));
        assert_lease_cleaned_up(&broker.catalog, lease.id);
        broker.catalog.assert_claims_consistent();
        assert!(broker.drop_lease(lease.id).is_err());

        assert_data_tree!(inspect, root: contains {
            test: contains {
                leases: {},
            },
        });

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    create_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        level: ON.level as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::Pending.into_primitive() as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::PoweringDown.into_primitive() as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
                {
                    rm_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        name: "L",
                        level: ON.level as u64,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
                {
                    rm_element: {
                        element_id: *element_b,
                        name: "B",
                    },
                },
            ]
        );
    }

    // When an element is removed while a lease targeting it is active (Satisfied),
    // the lease should be dropped immediately, its claims cleaned up, its entries
    // removed from `leases` and `lease_status`, and its Inspect lifecycle events
    // emitted before the element vertex is removed.
    #[fuchsia::test]
    async fn test_remove_element_with_active_lease() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a =
            broker.add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![]).unwrap();
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_a.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();

        let lease =
            broker.acquire_lease(element_b, ON, zx::Koid::from_raw(456)).expect("acquire failed");

        broker.update_current_level(element_a, ON);
        broker.update_current_level(element_b, ON);
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        broker.remove_element(&element_b);
        assert!(!broker.element_exists(element_b));
        assert_eq!(broker.get_required_level(&element_a), Some(OFF));
        assert_lease_cleaned_up(&broker.catalog, lease.id);
        broker.catalog.assert_claims_consistent();
        assert!(broker.drop_lease(lease.id).is_err());

        assert_data_tree!(inspect, root: contains {
            test: contains {
                leases: {},
            },
        });

        let hierarchy = fuchsia_inspect::reader::read(&inspect).await.unwrap();
        assert_events_recorded_in_order!(
            &hierarchy,
            [
                {
                    create_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        level: ON.level as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::Pending.into_primitive() as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::Satisfied.into_primitive() as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::PoweringDown.into_primitive() as u64,
                    },
                },
                {
                    update_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
                {
                    rm_lease: {
                        element_id: *element_b,
                        lease_id: *lease.id,
                        level: ON.level as u64,
                        status: LeaseStatus::Vacated.into_primitive() as u64,
                    },
                },
                {
                    rm_element: {
                        element_id: *element_b,
                        name: "B",
                    },
                },
            ]
        );
    }

    // When a target element is removed while a multi-hop lease targeting it is
    // in the process of being fulfilled (with pending claims on the removed
    // element and active/in-transition claims several hops away), dropping the
    // lease during element removal must also clean up its orphaned claims
    // several hops away and vacate the lease immediately.
    #[fuchsia::test]
    async fn test_remove_element_with_multihop_pending_lease() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        // D(ON) -> C(ON) -> B(ON) -> A(ON).
        let element_a =
            broker.add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![]).unwrap();
        let token_a = DependencyToken::create();
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_b = broker
            .add_element(
                "B",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_a.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();
        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_c = broker
            .add_element(
                "C",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_b.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();
        let token_c = DependencyToken::create();
        broker
            .register_dependency_token(
                element_c,
                token_c.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into(),
            )
            .unwrap();

        let element_d = broker
            .add_element(
                "D",
                OFF.level,
                BINARY_POWER_LEVELS.to_vec(),
                vec![fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_c.into()),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                }],
            )
            .unwrap();

        // Acquire a lease on D(ON). Initially `[B(ON) -> A(ON)]` is activated (requiring A(ON)),
        // while `[C(ON) -> B(ON)]`, `[D(ON) -> C(ON)]`, and `[None -> D(ON)]` are pending.
        let lease =
            broker.acquire_lease(element_d, ON, zx::Koid::from_raw(789)).expect("acquire failed");
        assert_eq!(broker.get_required_level(&element_a), Some(ON));
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));

        // Transition A to ON. This satisfies `[B(ON) -> A(ON)]` and activates `[C(ON) -> B(ON)]`
        // (putting B into transition to ON), while `[D(ON) -> C(ON)]` and `[None -> D(ON)]`
        // remain pending because B has not yet reached ON.
        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_required_level(&element_b), Some(ON));
        assert_eq!(broker.get_required_level(&element_c), Some(OFF));
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Pending));

        // Remove D while `[D(ON) -> C(ON)]` and `[None -> D(ON)]` are still pending. `drop_lease`
        // drops those pending claims on D immediately (leaving no claims referencing D in
        // `catalog.claims`), so `find_leases_affected_by_removal` must find `lease` via
        // `catalog.leases` and `cleanup_orphaned_claims_for_leases` must drop the orphaned
        // `[C(ON) -> B(ON)]` and `[B(ON) -> A(ON)]` claims and vacate `lease` immediately.
        broker.remove_element(&element_d);
        assert!(!broker.element_exists(element_d));
        assert_eq!(broker.get_required_level(&element_a), Some(OFF));
        assert_lease_cleaned_up(&broker.catalog, lease.id);
        broker.catalog.assert_claims_consistent();

        // Complete B's in-flight transition to ON; now that `[C(ON) -> B(ON)]` has been dropped,
        // B's required level drops back to OFF.
        broker.update_current_level(element_b, ON);
        assert_eq!(broker.get_required_level(&element_b), Some(OFF));
    }

    #[fuchsia::test]
    fn test_update_leases_for_dependency_active() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a = broker
            .add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        let element_b = broker
            .add_element("B", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");

        // Create a lease for A at ON and verify that A's required level is now ON.
        let lease =
            broker.acquire_lease(element_a, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        assert_eq!(broker.get_required_level(&element_a), Some(ON));

        // Update current level to ON and verify the lease is satisfied.
        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));
        let pending_claims = broker.catalog.pending_for_lease(lease.id);
        assert_eq!(pending_claims.len(), 0);
        let enforced_claims: Vec<_> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        assert_eq!(enforced_claims.len(), 1);
        let claim_a = &enforced_claims[0];
        assert_eq!(claim_a.requires.element_id, element_a);
        assert_eq!(claim_a.requires.level, ON);

        // Now add a dependency: A at ON requires B at ON.
        let dependency = Dependency {
            dependent: Some(ElementLevel { element_id: element_a, level: ON }),
            requires: ElementLevel { element_id: element_b, level: ON },
        };
        broker.update_leases_for_dependency(dependency.clone());

        // Verify that a claim for B at ON is added to the lease (and no other claims were added).
        let pending_claims = broker.catalog.pending_for_lease(lease.id);
        assert_eq!(pending_claims.len(), 0);
        let enforced_claims: Vec<_> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        assert_eq!(enforced_claims.len(), 2);

        // One claim should be for A at ON and one for B at ON.
        let found_a = enforced_claims
            .iter()
            .any(|dep| dep.requires.element_id == element_a && dep.requires.level == ON);
        let found_b = enforced_claims.iter().any(|dep| *dep == dependency);
        assert!(found_a, "Claim for A at ON not found in enforced claims");
        assert!(found_b, "Claim for B at ON not found in enforced claims");
    }

    #[fuchsia::test]
    fn test_update_leases_for_dependency_unaffected() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let levels = vec![0, 1, 2];
        let level_0 = IndexedPowerLevel { level: 0, index: 0 };
        let level_1 = IndexedPowerLevel { level: 1, index: 1 };
        let level_2 = IndexedPowerLevel { level: 2, index: 2 };

        let element_a = broker
            .add_element("A", level_0.level, levels.clone(), vec![])
            .expect("add_element failed");
        let element_b = broker
            .add_element("B", level_0.level, levels.clone(), vec![])
            .expect("add_element failed");

        // Create a lease for A at 1.
        let lease = broker
            .acquire_lease(element_a, level_1, zx::Koid::from_raw(1))
            .expect("acquire failed");

        // There should be only one claim for A at 1.
        let pending_claims = broker.catalog.pending_for_lease(lease.id);
        let enforced_claims: Vec<_> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        let all_claims: Vec<_> =
            pending_claims.into_iter().chain(enforced_claims.into_iter()).collect();
        assert_eq!(all_claims.len(), 1);
        assert_eq!(all_claims[0].requires.element_id, element_a);
        assert_eq!(all_claims[0].requires.level, level_1);

        // Now add dependency: A at 2 requires B at 1.
        // Since A is only leased at 1, the lease should not be affected.
        let dependency = Dependency {
            dependent: Some(ElementLevel { element_id: element_a, level: level_2 }),
            requires: ElementLevel { element_id: element_b, level: level_1 },
        };
        broker.update_leases_for_dependency(dependency);

        // Verify that NO new claim is added.
        let pending_claims = broker.catalog.pending_for_lease(lease.id);
        let enforced_claims: Vec<_> =
            broker.catalog.enforced_for_lease(lease.id).cloned().collect();
        let all_claims: Vec<_> =
            pending_claims.into_iter().chain(enforced_claims.into_iter()).collect();
        assert_eq!(all_claims.len(), 1);
        assert_eq!(all_claims[0].requires.element_id, element_a);
        assert_eq!(all_claims[0].requires.level, level_1);
    }

    #[fuchsia::test]
    fn test_removable_dependency() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let token_a = DependencyToken::create();
        let element_a = broker.add_element("A", 0, vec![0, 1], vec![]).expect("add_element failed");
        broker
            .register_dependency_token(
                element_a,
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register failed");

        let dep = fpb::LevelDependency {
            dependent_level: Some(1),
            requires_token: Some(
                token_a.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
            ),
            requires_level_by_preference: Some(vec![1]),
            remove_with_required_element: Some(true),
            ..Default::default()
        };

        let element_b =
            broker.add_element("B", 0, vec![0, 1], vec![dep]).expect("add_element failed");

        let lease = broker
            .acquire_lease(
                element_b,
                IndexedPowerLevel { level: 1, index: 1 },
                zx::Koid::from_raw(1),
            )
            .expect("acquire failed");

        // B's lease is pending A(1) being satisfied. B's required level is still 0.
        assert_eq!(broker.get_required_level(&element_b).unwrap().level, 0);
        // A's required level has been raised to 1 due to B's dependency.
        assert_eq!(broker.get_required_level(&element_a).unwrap().level, 1);

        // Transition A to 1.
        broker.update_current_level(element_a, IndexedPowerLevel { level: 1, index: 1 });

        // Now B's dependency is satisfied, so B's required level is 1.
        assert_eq!(broker.get_required_level(&element_b).unwrap().level, 1);

        // Transition B to 1 to satisfy the lease.
        broker.update_current_level(element_b, IndexedPowerLevel { level: 1, index: 1 });
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));

        broker.remove_element(&element_a);

        // Since the dependency was removable, B's required level should not be dropped to minimum/unsatisfiable.
        assert_eq!(broker.get_required_level(&element_b).unwrap().level, 1);
        // The lease should still be satisfied.
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));
    }

    #[fuchsia::test]
    fn test_lease_powering_down_and_vacated_states() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a = broker
            .add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");
        let element_b = broker
            .add_element("B", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");

        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        // A at ON requires B at ON.
        broker
            .add_dependency(
                element_a,
                fpb::LevelDependency {
                    dependent_level: Some(ON.level),
                    requires_token: Some(token_b),
                    requires_level_by_preference: Some(vec![ON.level]),
                    ..Default::default()
                },
                &mut EagerInspectWriter,
            )
            .expect("add_dependency failed");

        // Acquire lease on A at ON.
        let lease =
            broker.acquire_lease(element_a, ON, zx::Koid::from_raw(1)).expect("acquire failed");

        // Satisfy dependencies.
        broker.update_current_level(element_b, ON);
        broker.update_current_level(element_a, ON);

        // Verify lease is Satisfied.
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::Satisfied));
        assert!(broker.catalog.leases.contains_key(&lease.id));

        // Drop the lease.
        broker.drop_lease(lease.id).expect("drop failed");

        // Verify it enters POWERING_DOWN state and is not yet deleted.
        assert_eq!(broker.get_lease_status(lease.id), Some(LeaseStatus::PoweringDown));
        assert!(broker.catalog.leases.contains_key(&lease.id));

        // Let's power down element A to OFF. This should cascade and drop claims.
        broker.update_current_level(element_a, OFF);

        // The lease should be VACATED now, but we can't observe that as it has
        // been cleaned up.
        assert_eq!(broker.get_lease_status(lease.id), None);
        assert!(!broker.catalog.leases.contains_key(&lease.id));
    }

    #[fuchsia::test]
    fn test_stranded_powering_down_lease() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a = broker
            .add_element("A", OFF.level, vec![0, 10, 20], vec![])
            .expect("add_element failed");
        let element_b = broker
            .add_element("B", OFF.level, vec![0, 10, 20], vec![])
            .expect("add_element failed");

        let token_b = DependencyToken::create();
        broker
            .register_dependency_token(
                element_b,
                token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed").into(),
            )
            .expect("register_dependency_token failed");

        // A     B
        // 20 -> 20
        // 10 -> 10
        broker
            .add_dependency(
                element_a,
                fpb::LevelDependency {
                    dependent_level: Some(10),
                    requires_token: Some(
                        token_b.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    ),
                    requires_level_by_preference: Some(vec![10]),
                    ..Default::default()
                },
                &mut EagerInspectWriter,
            )
            .expect("add_dependency failed");
        broker
            .add_dependency(
                element_a,
                fpb::LevelDependency {
                    dependent_level: Some(20),
                    requires_token: Some(token_b),
                    requires_level_by_preference: Some(vec![20]),
                    ..Default::default()
                },
                &mut EagerInspectWriter,
            )
            .expect("add_dependency failed");

        let level_10 = IndexedPowerLevel { level: 10, index: 1 };
        let level_20 = IndexedPowerLevel { level: 20, index: 2 };

        // Acquire Lease 1 on A at level 10.
        let lease1 =
            broker.acquire_lease(element_a, level_10.clone(), zx::Koid::from_raw(1)).unwrap();
        broker.update_current_level(element_b, level_10.clone());
        broker.update_current_level(element_a, level_10.clone());
        assert_eq!(broker.get_lease_status(lease1.id), Some(LeaseStatus::Satisfied));

        // Acquire Lease 2 on A at level 10.
        let lease2 =
            broker.acquire_lease(element_a, level_10.clone(), zx::Koid::from_raw(2)).unwrap();
        assert_eq!(broker.get_lease_status(lease2.id), Some(LeaseStatus::Satisfied));

        // Bring B to level 20 first so that Lease 3's top claim can be
        // activated immediately.
        broker.update_current_level(element_b, level_20.clone());

        // Acquire Lease 3 on A at level 20. This puts element A into
        // in_transition while transiting to 20.
        let lease3 =
            broker.acquire_lease(element_a, level_20.clone(), zx::Koid::from_raw(3)).unwrap();

        // While A is in transition, drop Lease 2.
        // Because A is in transition, Lease 2's bottom claim (A@10 -> B@10)
        // cannot be dropped safely, leaving Lease 2 stranded in PoweringDown
        // until the transition is complete.
        broker.drop_lease(lease2.id).unwrap();
        assert_eq!(broker.get_lease_status(lease2.id), Some(LeaseStatus::PoweringDown));

        // Complete transition of A to level 20.
        broker.update_current_level(element_a, level_20.clone());
        assert_eq!(broker.get_lease_status(lease3.id), Some(LeaseStatus::Satisfied));

        // Now that A is at level 20 and Lease 1 and Lease 3 are SATISFIED,
        // Lease 2's claims are redundantly covered and Lease 2 is cleanly
        // vacated (None).
        assert_eq!(broker.get_lease_status(lease2.id), None);

        // Now drop Lease 3 and step A back down to level 10.
        broker.drop_lease(lease3.id).unwrap();
        broker.update_current_level(element_a, level_10.clone());
        assert_eq!(broker.get_lease_status(lease1.id), Some(LeaseStatus::Satisfied));
        assert_eq!(broker.get_lease_status(lease3.id), None);
    }

    #[fuchsia::test]
    fn test_overlapping_leases_vacate_immediately() {
        let inspect = fuchsia_inspect::Inspector::default();
        let mut broker = Broker::new(inspect.root().create_child("test"));

        let element_a = broker
            .add_element("A", OFF.level, BINARY_POWER_LEVELS.to_vec(), vec![])
            .expect("add_element failed");

        let lease1 =
            broker.acquire_lease(element_a, ON, zx::Koid::from_raw(1)).expect("acquire failed");
        let lease2 =
            broker.acquire_lease(element_a, ON, zx::Koid::from_raw(2)).expect("acquire failed");

        broker.update_current_level(element_a, ON);
        assert_eq!(broker.get_lease_status(lease1.id), Some(LeaseStatus::Satisfied));
        assert_eq!(broker.get_lease_status(lease2.id), Some(LeaseStatus::Satisfied));

        // Dropping Lease 1 while Lease 2 is active should immediately vacate Lease 1
        // because Lease 2's active claim supports element_a at ON.
        broker.drop_lease(lease1.id).expect("drop failed");
        assert_eq!(broker.get_lease_status(lease1.id), None);
        assert!(!broker.catalog.leases.contains_key(&lease1.id));

        assert_eq!(broker.get_lease_status(lease2.id), Some(LeaseStatus::Satisfied));
    }
}
