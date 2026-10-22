// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InterfaceId;
use crate::dns::DNS_PORT;
use crate::telemetry::{NetworkEventMetadata, TelemetryEvent, TelemetrySender};
use anyhow::Context as _;
use assert_matches::assert_matches;
use async_utils::stream::{StreamItem, StreamWithEpitaph, Tagged, WithEpitaph as _, WithTag as _};
use dns_server_watcher::DnsServers;
use fidl::endpoints::{ControlHandle as _, RequestStream as _};
use futures::StreamExt as _;
use log::{debug, error, info, warn};
use policy_properties::NetworkTokenExt as _;
use std::collections::HashMap;
use std::collections::hash_map::{Entry, OccupiedEntry};

mod reachability;
mod token_registry;

use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_ext as fnet_ext;
use fidl_fuchsia_net_name as fnet_name;
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fidl_fuchsia_net_reachability as freachability;
use fidl_fuchsia_posix_socket as fposix_socket;
use fuchsia_inspect::Property as _;
use fuchsia_inspect_derive::{IValue, Inspect, Unit, WithInspect as _};
pub use reachability::ReachabilityWatcherConnectionId;
use reachability::{ReachabilityHandler, ReachabilityStream};

// The id for each network, separated by network source.
//
// NB: These are separated in the case that the same underlying
// interface id is used by Fuchsia and a delegated actor.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkId {
    Fuchsia(InterfaceId),
    Delegated(InterfaceId),
}

impl std::fmt::Display for NetworkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkId::Fuchsia(interface_id) => write!(f, "fuchsia:{interface_id}"),
            NetworkId::Delegated(interface_id) => write!(f, "delegated:{interface_id}"),
        }
    }
}

impl NetworkId {
    pub fn get(&self) -> InterfaceId {
        match self {
            NetworkId::Fuchsia(interface_id) => *interface_id,
            NetworkId::Delegated(interface_id) => *interface_id,
        }
    }

    pub fn fuchsia<I: Into<InterfaceId>>(id: I) -> Self {
        NetworkId::Fuchsia(id.into())
    }

    pub fn delegated<I: Into<InterfaceId>>(id: I) -> Self {
        NetworkId::Delegated(id.into())
    }

    pub fn is_fuchsia(&self) -> bool {
        matches!(self, NetworkId::Fuchsia(_))
    }
}

// Using the interface's current v4/v6 default route properties and v4/v6 default route
// properties provided through `fnet_interfaces_ext::Event::Changed`, determine
// whether the interface gained or lost candidacy because of the event, or maintained
// the same state.
//
// Returns None when the candidacy has not changed, Some(true) when the interface
// gains candidacy, and Some(false) when the interface loses candidacy.
pub(crate) fn determine_interface_state_changed(
    prev: &fidl_fuchsia_net_interfaces::Properties,
    curr: &fidl_fuchsia_net_interfaces_ext::Properties<
        fidl_fuchsia_net_interfaces_ext::DefaultInterest,
    >,
) -> Option<bool> {
    let was_candidate = (prev.has_default_ipv4_route.unwrap_or(curr.has_default_ipv4_route)
        || prev.has_default_ipv6_route.unwrap_or(curr.has_default_ipv6_route))
        && prev.online.unwrap_or(curr.online);
    let is_candidate = (curr.has_default_ipv4_route || curr.has_default_ipv6_route) && curr.online;
    (is_candidate != was_candidate).then_some(is_candidate)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NetworkTokenContents {
    network_id: NetworkId,
    is_default: bool,
}

/// Declares a connection id and the allocator that mints it.
///
/// The id wraps a private counter, so the allocator is the only way to construct one outside the
/// declaring module. Request handlers rely on that: an id that exists came from an allocator, so
/// it is always present in the map it indexes.
macro_rules! connection_id {
    ($id:ident => $allocator:ident) => {
        /// Identifies a single client connection.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $id(usize);

        #[doc = concat!("Mints unique [`", stringify!($id), "`]s.")]
        #[derive(Default)]
        pub struct $allocator(usize);

        impl $allocator {
            pub fn allocate(&mut self) -> $id {
                let id = $id(self.0);
                self.0 += 1;
                id
            }
        }
    };
}
pub(crate) use connection_id;

mod id {
    // A `fuchsia.net.policy.properties.Networks` client connection.
    connection_id!(NetworksConnectionId => NetworksConnectionIdAllocator);

    // A `fuchsia.net.policy.properties.PropertyWatcher` client connection.
    connection_id!(PropertyWatcherConnectionId => PropertyWatcherConnectionIdAllocator);
}

pub use id::{NetworksConnectionId, PropertyWatcherConnectionId};
use id::{NetworksConnectionIdAllocator, PropertyWatcherConnectionIdAllocator};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UpdateGeneration {
    /// The current generation for `fuchsia.net.policy.properties.WatchDefault`.
    /// Incremented each time the default network changes.
    default_network: usize,

    /// The current generation for `fuchsia.net.policy.properties.WatchProperties`.
    /// Incremented each time a network property changes.
    properties: usize,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateGenerations {
    default_network: HashMap<NetworksConnectionId, usize>,
    properties: HashMap<PropertyWatcherConnectionId, usize>,
}

impl UpdateGenerations {
    fn default_network(&self, id: &NetworksConnectionId) -> Option<usize> {
        self.default_network.get(id).copied()
    }

    fn set_default_network(&mut self, id: NetworksConnectionId, generation: UpdateGeneration) {
        *self.default_network.entry(id).or_default() = generation.default_network;
    }

    fn remove_default(&mut self, id: &NetworksConnectionId) -> Option<usize> {
        self.default_network.remove(id)
    }

    fn properties(&self, id: &PropertyWatcherConnectionId) -> Option<usize> {
        self.properties.get(id).copied()
    }

    fn set_properties(&mut self, id: PropertyWatcherConnectionId, generation: UpdateGeneration) {
        *self.properties.entry(id).or_default() = generation.properties;
    }

    fn remove_properties(&mut self, id: &PropertyWatcherConnectionId) -> Option<usize> {
        self.properties.remove(id)
    }
}

trait SetMark {
    fn set_mark(&mut self, domain: fnet::MarkDomain, value: Option<u32>);
}

impl SetMark for fnet::Marks {
    fn set_mark(&mut self, domain: fnet::MarkDomain, value: Option<u32>) {
        match domain {
            fnet::MarkDomain::Mark1 => self.mark_1 = value,
            fnet::MarkDomain::Mark2 => self.mark_2 = value,
        }
    }
}

/// State for a registered `fuchsia.net.policy.properties.PropertyWatcher` client,
/// including the network token, properties being watched, and any pending `Watch` responder.
#[derive(Debug)]
struct Registration {
    token: fnp_properties::NetworkToken,
    properties: fnp_properties::PropertyInterest,
    responder: Option<fnp_properties::PropertyWatcherWatchResponder>,
    /// Used to close the connection. Closing guarantees the watcher's [`ConnectionStream`]
    /// yields the terminal item that this registration is removed on.
    control_handle: fnp_properties::PropertyWatcherControlHandle,
}

impl Registration {
    /// Closes the watcher's connection, with `epitaph` if one is provided.
    ///
    /// The registration is left in place: shutting the connection down guarantees its
    /// [`ConnectionStream`] yields the terminal item that the registration is removed on.
    fn close(&mut self, epitaph: Option<zx::Status>) {
        // Shut down before releasing the responder below to communicate an epitaph
        // when one is provided.
        match epitaph {
            Some(status) => self.control_handle.shutdown_with_epitaph(status),
            None => self.control_handle.shutdown(),
        }
        // Drop the hanging responder, if any, so a closed watcher is no longer served.
        self.responder = None;
    }
}

/// State for a connected `fuchsia.net.policy.properties.Networks` client.
#[derive(Debug)]
struct NetworksClient {
    /// The responder for a hanging `WatchDefault` call, if one is outstanding.
    responder: Option<fnp_properties::NetworksWatchDefaultResponder>,
    /// Used to close the connection. Closing guarantees the client's [`ConnectionStream`]
    /// yields the terminal item that this entry is removed on.
    control_handle: fnp_properties::NetworksControlHandle,
}

impl NetworksClient {
    /// Closes the client's connection, with `epitaph` if one is provided.
    ///
    /// The client is left in place: shutting the connection down guarantees its
    /// [`ConnectionStream`] yields the terminal item that the client is removed on.
    fn close(&mut self, epitaph: Option<zx::Status>) {
        // Shut down before releasing the responder below to communicate an epitaph
        // when one is provided.
        match epitaph {
            Some(status) => self.control_handle.shutdown_with_epitaph(status),
            None => self.control_handle.shutdown(),
        }
        // Drop the hanging responder, if any, so a closed client is no longer served.
        self.responder = None;
    }
}

#[derive(Debug, PartialEq, Default, Clone)]
struct NetworkProperties {
    delegated: Option<DelegatedNetworkProperties>,
    dns_servers: Vec<fnet_name::DnsServer_>,
    // TODO(https://fxbug.dev/567965129): Remove the following optional fields for shared
    // Fuchsia/Starnix networks once all networks received from Starnix are required to
    // have these fields.
    connectivity_state: Option<fnp_properties::ConnectivityState>,
    name: Option<String>,
    network_type: Option<fnp_properties::NetworkType>,
}

impl NetworkProperties {
    fn get_marks(&self) -> Option<&fnet::Marks> {
        self.delegated.as_ref().map(|d| &d.marks)
    }
}

impl std::fmt::Display for NetworkProperties {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let NetworkProperties { name, network_type, connectivity_state, delegated, dns_servers } =
            self;

        if let Some(name) = name {
            write!(f, "name={name}")?;
        }
        if let Some(network_type) = network_type {
            write!(f, ", type={}", network_type.as_str())?;
        }
        if let Some(connectivity_state) = connectivity_state {
            write!(f, ", state={}", connectivity_state.as_str())?;
        }
        if let Some(DelegatedNetworkProperties { marks, starnix_handle: _ }) = delegated {
            write!(f, ", mark={:?}", marks.mark_1)?;
        }
        if delegated.is_some() || !dns_servers.is_empty() {
            write!(f, ", dns=[")?;
            let mut servers = dns_servers.iter().filter_map(|s| {
                s.address.map(|a| fidl_fuchsia_net_ext::SocketAddress::from(a).0.ip().to_string())
            });
            if let Some(first) = servers.next() {
                write!(f, "{first}")?;
                for s in servers {
                    write!(f, ", {s}")?;
                }
            }
            write!(f, "]")?;
        }
        Ok(())
    }
}

const NETWORK_REGISTRY_LOG_TAG: &str = "network_registry";

/// The current state of all networks sent to the NetworkRegistry.
#[derive(Default, Clone)]
struct RegisteredNetworks {
    /// The current default network, determined by the priority rules in
    /// `calculate_active_default`.
    default_network: Option<NetworkId>,
    /// The Starnix default network, as determined by Starnix.
    starnix_default: Option<NetworkId>,
    networks: HashMap<NetworkId, NetworkProperties>,
    dns_servers: Vec<fnet_name::DnsServer_>,
}

fn format_opt_network_id(id: Option<NetworkId>) -> String {
    match id {
        Some(id) => id.to_string(),
        None => "None".to_string(),
    }
}

impl RegisteredNetworks {
    fn set_starnix_default(&mut self, next_starnix_default: Option<NetworkId>) {
        if self.starnix_default != next_starnix_default {
            info!(
                tag = NETWORK_REGISTRY_LOG_TAG;
                "SetStarnixDefault {} (prev={})",
                format_opt_network_id(next_starnix_default),
                format_opt_network_id(self.starnix_default),
            );
            self.starnix_default = next_starnix_default;
        }
    }

    // Determine the active default network based on the starnix_default and network registry.
    // When one or more Fuchsia networks are present, they should be prioritized over Starnix
    // networks. The 'most prioritized' Fuchsia network is the one with the lowest ID.
    fn calculate_active_default(&self) -> Option<NetworkId> {
        // Note: Fuchsia networks are only added to the NetworkRegistry if they meet
        // certain criteria (ex: have a default route and are online).
        let first_fuchsia = self.networks.keys().filter(|id| id.is_fuchsia()).cloned().min();
        if let Some(fd) = first_fuchsia {
            return Some(fd);
        }

        // Fallback to starnix_default. If it is unset, no default network is available.
        if let Some(starnix_default) = self.starnix_default {
            // Ensure that the network is present in the network registry.
            assert!(self.networks.contains_key(&starnix_default));
        }
        self.starnix_default
    }

    // Handle updates to the active default network.
    //
    // Returns `Some(DefaultChangedEvent)` if the new default network
    // is different from the old one, otherwise `None`.
    fn handle_default_network_update(&mut self) -> Option<DefaultChangedEvent> {
        let next_default = self.calculate_active_default();
        if next_default != self.default_network {
            let old_default = self.default_network;
            info!(
                tag = NETWORK_REGISTRY_LOG_TAG;
                "SetDefault {} (prev={})",
                format_opt_network_id(next_default),
                format_opt_network_id(old_default),
            );
            self.default_network = next_default;
            Some(DefaultChangedEvent { previous_default: old_default })
        } else {
            None
        }
    }

    fn apply(&mut self, update: NetworkRegistryUpdate) -> RegistryUpdateResult {
        match update {
            NetworkRegistryUpdate::UnsetDefaultNetwork => {
                // Handle Starnix unsetting its default network.
                self.set_starnix_default(None);
                RegistryUpdateResult {
                    event: UpdateApplied::None,
                    default_changed: self.handle_default_network_update(),
                }
            }
            NetworkRegistryUpdate::ChangeNetwork(network_id, network_change) => {
                match network_change {
                    NetworkUpdate::Properties(event) => RegistryUpdateResult {
                        event: self.handle_changed_network(network_id, event),
                        default_changed: self.handle_default_network_update(),
                    },
                    NetworkUpdate::Remove => {
                        if self.starnix_default == Some(network_id) {
                            error!("Cannot remove the default delegated network. Update ignored.");
                            RegistryUpdateResult {
                                event: UpdateApplied::None,
                                default_changed: None,
                            }
                        } else if let Some(properties) = self.networks.remove(&network_id) {
                            info!(
                                tag = NETWORK_REGISTRY_LOG_TAG;
                                "Remove {network_id} ({properties})"
                            );
                            // Elect fallback default network internally.
                            RegistryUpdateResult {
                                event: UpdateApplied::NetworkRemoved(network_id),
                                default_changed: self.handle_default_network_update(),
                            }
                        } else {
                            error!(
                                "Cannot remove a non-existent network ({network_id:?}). \
                                Update ignored."
                            );
                            RegistryUpdateResult {
                                event: UpdateApplied::None,
                                default_changed: None,
                            }
                        }
                    }
                    NetworkUpdate::MakeDefault => {
                        match network_id {
                            // Fuchsia networks are always the default network when present. Netcfg
                            // does not use this API to set a Fuchsia network as the default.
                            NetworkId::Fuchsia(_) => {}
                            NetworkId::Delegated(_) => self.set_starnix_default(Some(network_id)),
                        }
                        let default_changed = self.handle_default_network_update();
                        RegistryUpdateResult { event: UpdateApplied::None, default_changed }
                    }
                }
            }
            NetworkRegistryUpdate::UpdateDns(dns_servers) => {
                let event = if self.dns_servers != dns_servers {
                    self.dns_servers = dns_servers;
                    UpdateApplied::DnsChanged
                } else {
                    UpdateApplied::None
                };
                RegistryUpdateResult { event, default_changed: None }
            }
        }
    }

    // Handle the `NetworkPropertiesChange` in a `NetworkRegistryUpdate`, determining
    // whether network properties changed as a result of the update.
    //
    // Returns an `UpdateApplied::NetworkChanged` event if this is a valid change.
    fn handle_changed_network(
        &mut self,
        network_id: NetworkId,
        event: NetworkPropertiesChange,
    ) -> UpdateApplied {
        let NetworkPropertiesChange {
            added,
            delegated,
            dns_servers: changed_dns_servers,
            connectivity_state,
            name,
            network_type,
        } = event;
        let entry = self.networks.entry(network_id);
        let result = match (added, &entry, network_id, &delegated) {
            (true, Entry::Occupied(_), _, _) => Err("add already added network"),
            (false, Entry::Vacant(_), _, _) => Err("update a non-added network"),
            (_, _, NetworkId::Fuchsia(_), Some(_)) => {
                Err("have a fuchsia network with delegated properties")
            }
            (_, _, NetworkId::Delegated(_), None) => {
                Err("have a delegated network without delegated properties")
            }
            (_, _, NetworkId::Fuchsia(_), None) | (_, _, NetworkId::Delegated(_), Some(_)) => {
                Ok(())
            }
        };
        if let Err(e) = result {
            error!("Cannot {e}. Update ignored.");
            return UpdateApplied::None;
        }

        let dns_servers = changed_dns_servers.unwrap_or_default();
        let (changed_marks, changed_dns) = match &entry {
            Entry::Occupied(e) => (
                e.get().get_marks() != delegated.as_ref().map(|d| &d.marks),
                e.get().dns_servers != dns_servers,
            ),
            // When adding a new network, set both `changed_marks` and `changed_dns` to
            // true so responders receive an explicit initial state.
            Entry::Vacant(_) => (true, true),
        };
        let properties = NetworkProperties {
            delegated,
            dns_servers,
            connectivity_state,
            name: name.clone(),
            network_type,
        };
        match &entry {
            Entry::Occupied(e) => {
                // Only log network updates when they have changed.
                if *e.get() != properties {
                    info!(
                        tag = NETWORK_REGISTRY_LOG_TAG; "Update {network_id} ({properties})"
                    );
                }
            }
            Entry::Vacant(_) => {
                info!(tag = NETWORK_REGISTRY_LOG_TAG; "Add {network_id} ({properties})");
            }
        }
        // Validation above ensures adds (`added`) only insert into a `Vacant` entry and updates
        // (`!added`) replace an `Occupied` entry.
        let _: OccupiedEntry<'_, _, _> = entry.insert_entry(properties);
        UpdateApplied::NetworkChanged {
            network_id,
            added,
            changed_marks,
            changed_dns,
            name,
            network_type,
        }
    }

    /// Returns the DNS servers for the default network if it is a Fuchsia network,
    /// otherwise returns a concatenation of DNS servers from all delegated networks.
    /// TODO(https://fxbug.dev/428712735): Remove once dns-resolver learns about DNS
    /// via NetworkProperties.
    pub fn consolidated_dns_servers(&self) -> Vec<fnet_name::DnsServer_> {
        if let Some(NetworkId::Fuchsia(if_id)) = self.default_network {
            self.networks
                .get(&NetworkId::Fuchsia(if_id))
                .map(|p| p.dns_servers.clone())
                .unwrap_or_default()
        } else {
            self.networks
                .iter()
                .filter(|(id, _)| matches!(id, NetworkId::Delegated(_)))
                .flat_map(|(_, p)| &p.dns_servers)
                .cloned()
                .collect()
        }
    }

    fn dns_servers_for_network<'a>(
        &'a self,
        network_id: NetworkId,
        properties: &'a NetworkProperties,
    ) -> Vec<&'a fnet_name::DnsServer_> {
        match network_id {
            NetworkId::Delegated(_) => properties.dns_servers.iter().collect(),
            NetworkId::Fuchsia(InterfaceId(if_id)) => {
                if !properties.dns_servers.is_empty() {
                    properties.dns_servers.iter().collect()
                } else {
                    self.dns_servers
                        .iter()
                        .filter(|s| match &s.source {
                            Some(fnet_name::DnsServerSource::Dhcp(d)) => {
                                d.source_interface == Some(if_id.get())
                            }
                            Some(fnet_name::DnsServerSource::Dhcpv6(d)) => {
                                d.source_interface == Some(if_id.get())
                            }
                            Some(fnet_name::DnsServerSource::Ndp(d)) => {
                                d.source_interface == Some(if_id.get())
                            }
                            _ => false,
                        })
                        .collect()
                }
            }
        }
    }

    fn update_inspect(&self, node: &fuchsia_inspect::Node) {
        node.clear_recorded();

        if let Some(default_network) = &self.default_network {
            node.record_string("default_network", default_network.to_string());
        }

        if let Some(starnix_default) = &self.starnix_default {
            node.record_string("starnix_default", starnix_default.to_string());
        }

        // The cross-source set handed to the resolver, including static servers no network
        // owns. Not `Self::consolidated_dns_servers`, the registry's SocketProxy contribution.
        if !self.dns_servers.is_empty() {
            node.record_child("system_dns_servers", |system_node| {
                for (i, server) in self.dns_servers.iter().enumerate() {
                    server.record_inspect(system_node, &i.to_string());
                }
            });
        }

        let mut sorted_networks: Vec<_> = self.networks.iter().collect();
        sorted_networks.sort_by_key(|(id, _)| *id);

        for (network_id, properties) in sorted_networks {
            let network_node_name = match network_id {
                NetworkId::Fuchsia(InterfaceId(id)) => format!("fuchsia_{id}"),
                NetworkId::Delegated(InterfaceId(id)) => {
                    format!("delegated_{id}")
                }
            };

            node.record_child(network_node_name, |net_node| {
                if let Some(name) = &properties.name {
                    net_node.record_string("name", name);
                }
                if let Some(network_type) = &properties.network_type {
                    net_node.record_string("network_type", network_type.as_str());
                }
                if let Some(connectivity_state) = &properties.connectivity_state {
                    net_node.record_string("connectivity_state", connectivity_state.as_str());
                }

                let network_dns_servers = self.dns_servers_for_network(*network_id, properties);

                if !network_dns_servers.is_empty() {
                    net_node.record_child("dns_servers", |dns_node| {
                        for (i, server) in network_dns_servers.iter().enumerate() {
                            server.record_inspect(dns_node, &i.to_string());
                        }
                    });
                }
            });
        }
    }
}

trait DnsServerInspectExt {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, name: &str);
}

impl DnsServerInspectExt for fnet_name::DnsServer_ {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, name: &str) {
        let Some(address) = &self.address else {
            return;
        };
        let server_str = fnet_ext::SocketAddress::from(*address).to_string();
        let (source_type, iface_id) = dns_source_info(&self.source);

        node.record_child(name, |entry_node| {
            entry_node.record_string("server", server_str);
            entry_node.record_string("type", source_type);
            if let Some(iface_id) = iface_id {
                entry_node.record_uint("interface_id", iface_id);
            }
        });
    }
}

trait NetworkTypeInspectExt {
    fn as_str(&self) -> &'static str;
}

impl NetworkTypeInspectExt for fnp_properties::NetworkType {
    fn as_str(&self) -> &'static str {
        match self {
            fnp_properties::NetworkType::Unknown => "Unknown",
            fnp_properties::NetworkType::Ethernet => "Ethernet",
            fnp_properties::NetworkType::Wifi => "Wifi",
            fnp_properties::NetworkType::Bluetooth => "Bluetooth",
            fnp_properties::NetworkType::Cellular => "Cellular",
            fnp_properties::NetworkType::__SourceBreaking { .. } => "Unknown",
        }
    }
}

trait ConnectivityStateInspectExt {
    fn as_str(&self) -> &'static str;
}

impl ConnectivityStateInspectExt for fnp_properties::ConnectivityState {
    fn as_str(&self) -> &'static str {
        match self {
            fnp_properties::ConnectivityState::NoConnectivity => "NoConnectivity",
            fnp_properties::ConnectivityState::LocalConnectivity => "LocalConnectivity",
            fnp_properties::ConnectivityState::PartialConnectivity => "PartialConnectivity",
            fnp_properties::ConnectivityState::FullConnectivity => "FullConnectivity",
            fnp_properties::ConnectivityState::__SourceBreaking { .. } => "Unknown",
        }
    }
}

fn dns_source_info(source: &Option<fnet_name::DnsServerSource>) -> (&'static str, Option<u64>) {
    match source {
        Some(fnet_name::DnsServerSource::StaticSource(_)) => ("Static", None),
        Some(fnet_name::DnsServerSource::Dhcp(dhcp)) => ("Dhcpv4", dhcp.source_interface),
        Some(fnet_name::DnsServerSource::Dhcpv6(dhcpv6)) => ("Dhcpv6", dhcpv6.source_interface),
        Some(fnet_name::DnsServerSource::Ndp(ndp)) => ("Ndp", ndp.source_interface),
        Some(fnet_name::DnsServerSource::SocketProxy(_)) => ("SocketProxy", None),
        Some(fnet_name::DnsServerSource::__SourceBreaking { .. }) | None => ("Unknown", None),
    }
}

/// Helper trait for building property update lists based on a client's registration.
trait PropertyUpdates {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        watched_properties: fnp_properties::PropertyInterest,
    );
    fn add_dns(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        watched_properties: fnp_properties::PropertyInterest,
    );
}

impl PropertyUpdates for fnp_properties::PropertyUpdate {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        watched_properties: fnp_properties::PropertyInterest,
    ) {
        if !watched_properties.contains(fnp_properties::PropertyInterest::SOCKET_MARKS) {
            return;
        }

        match network_registry.networks.get(&network.network_id) {
            Some(network) => {
                if let Some(socket_marks) = network.get_marks() {
                    self.socket_marks = Some(socket_marks.clone());
                }
                return;
            }
            None => {
                error!(
                    "State is inconsistent. We attempted to add marks for a \
            network that is not known: {:?}",
                    network.network_id
                );
            }
        }
    }

    fn add_dns(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        watched_properties: fnp_properties::PropertyInterest,
    ) {
        if !watched_properties.contains(fnp_properties::PropertyInterest::DNS_CONFIGURATION) {
            return;
        }

        let interface_id = network.network_id;
        self.dns_configuration = Some(fnp_properties::DnsConfiguration {
            servers: Some(
                network_registry
                    .dns_servers
                    .iter()
                    .filter(|d| {
                        match &d.source {
                            Some(source) => match source {
                                fnet_name::DnsServerSource::StaticSource(_) => true,
                                // `extract_dns_servers` prefers IPv4 DNS
                                // over IPv6 DNS when DNS servers are
                                // provided by the SocketProxy.
                                fnet_name::DnsServerSource::SocketProxy(
                                    fnet_name::SocketProxyDnsServerSource {
                                        source_interface, ..
                                    },
                                ) => match (interface_id, source_interface) {
                                    (_, None) => true,
                                    (id1, Some(id2)) => {
                                        Ok(id1)
                                            == InterfaceId::try_from(*id2)
                                                .map(|id| NetworkId::delegated(id))
                                    }
                                },
                                fnet_name::DnsServerSource::Dhcp(
                                    fnet_name::DhcpDnsServerSource { source_interface, .. },
                                )
                                | fnet_name::DnsServerSource::Ndp(
                                    fnet_name::NdpDnsServerSource { source_interface, .. },
                                )
                                | fnet_name::DnsServerSource::Dhcpv6(
                                    fnet_name::Dhcpv6DnsServerSource { source_interface, .. },
                                ) => match (interface_id, source_interface) {
                                    (_, None) => true,
                                    (id1, Some(id2)) => {
                                        Ok(id1)
                                            == InterfaceId::try_from(*id2)
                                                .map(|id| NetworkId::fuchsia(id))
                                    }
                                },

                                _ => {
                                    error!("unhandled DnsServerSource: {source:?}");
                                    false
                                }
                            },

                            // No source, assume static source, so include it.
                            None => true,
                        }
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            ..Default::default()
        });
    }
}

/// Delegated network specific properties.
///
/// Neither `marks` nor `starnix_handle` is expected to change for an existing
/// network, but since they are not managed by Fuchsia we update them on
/// `NetworkRegistry.Update` to guard against any assumptions about their
/// immutability. Updating `starnix_handle` only affects which handle future
/// `PrivilegedNetworkLookup.Lookup` calls match; any `NetworkToken` already
/// minted for the network is keyed by `NetworkId` and remains active until the
/// network itself is removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DelegatedNetworkProperties {
    /// The socket marks for the delegated network.
    pub marks: fnet::Marks,
    /// The Starnix network handle.
    pub starnix_handle: u64,
}

/// An event representing the properties that changed for a network.
#[derive(Clone, Debug, Default)]
pub struct NetworkPropertiesChange {
    /// When true, this is a new network being added. Otherwise, this is an
    /// update to an existing network.
    pub added: bool,
    /// Properties specific to delegated networks (`None` for Fuchsia networks).
    pub delegated: Option<DelegatedNetworkProperties>,
    /// If present, contains the new DNS servers for this network.
    pub dns_servers: Option<Vec<fnet_name::DnsServer_>>,
    /// The new connectivity state of the network.
    pub connectivity_state: Option<fnp_properties::ConnectivityState>,
    /// The name of the network.
    pub name: Option<String>,
    /// The transport type of the network.
    pub network_type: Option<fnp_properties::NetworkType>,
}

#[derive(Debug, Clone)]
pub enum NetworkUpdate {
    /// Change a network's properties.
    Properties(NetworkPropertiesChange),
    Remove,
    MakeDefault,
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct DefaultChangedEvent {
    previous_default: Option<NetworkId>,
}

#[derive(Debug, PartialEq, Eq)]
struct RegistryUpdateResult {
    event: UpdateApplied,
    /// Stores whether the default network has changed, and the previous default
    /// network, if any.
    default_changed: Option<DefaultChangedEvent>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum UpdateApplied {
    /// No update was performed.
    None,

    /// Whether the DNS servers changed.
    DnsChanged,

    /// Network was added or updated, contains the NetworkId of the added network.
    NetworkChanged {
        network_id: NetworkId,
        added: bool,
        changed_marks: bool,
        changed_dns: bool,
        name: Option<String>,
        network_type: Option<fnp_properties::NetworkType>,
    },

    /// Network was removed, contains the NetworkId of the removed network.
    NetworkRemoved(NetworkId),
}

#[derive(Debug, Clone)]
pub enum NetworkRegistryUpdate {
    UnsetDefaultNetwork,
    ChangeNetwork(NetworkId, NetworkUpdate),
    UpdateDns(Vec<fnet_name::DnsServer_>),
}

impl NetworkRegistryUpdate {
    pub fn unset_default() -> Self {
        NetworkRegistryUpdate::UnsetDefaultNetwork
    }

    pub fn dns(dns_servers: &DnsServers) -> Self {
        // TODO(https://fxbug.dev/477980011): Switch to deriving dns servers from
        // NetworkRegistry updates.
        NetworkRegistryUpdate::UpdateDns(dns_servers.consolidated_dns_servers())
    }
}

/// A validated copy of [`fnp_properties::Network`] ensuring required fields
/// are present for delegated (Starnix) networks.
///
/// Netcfg discovers native Fuchsia networks internally from Netstack, so
/// external `NetworkRegistry` requests handled by this type are strictly
/// delegated networks registered by runtimes like Starnix.
struct ValidatedNetwork {
    network_id: NetworkId,
    delegated: DelegatedNetworkProperties,
    dns_servers: Vec<fnet_name::DnsServer_>,
    connectivity: Option<fnp_properties::ConnectivityState>,
    name: Option<String>,
    network_type: Option<fnp_properties::NetworkType>,
}

#[derive(Debug, PartialEq, Eq)]
enum NetworkValidationError {
    MissingNetworkId,
    MissingNetworkInfo,
}

impl From<NetworkValidationError> for fnp_properties::NetworkRegistryAddError {
    fn from(error: NetworkValidationError) -> Self {
        match error {
            NetworkValidationError::MissingNetworkId => Self::MissingNetworkId,
            NetworkValidationError::MissingNetworkInfo => Self::MissingNetworkInfo,
        }
    }
}

impl From<NetworkValidationError> for fnp_properties::NetworkRegistryUpdateError {
    fn from(error: NetworkValidationError) -> Self {
        match error {
            NetworkValidationError::MissingNetworkId => Self::MissingNetworkId,
            NetworkValidationError::MissingNetworkInfo => Self::MissingNetworkInfo,
        }
    }
}

impl TryFrom<fnp_properties::Network> for ValidatedNetwork {
    type Error = NetworkValidationError;

    fn try_from(network: fnp_properties::Network) -> Result<Self, Self::Error> {
        let raw_network_id = network.network_id.ok_or(NetworkValidationError::MissingNetworkId)?;
        let network_id = InterfaceId::try_from(raw_network_id)
            .map(NetworkId::delegated)
            .map_err(|_| NetworkValidationError::MissingNetworkId)?;
        let Some(fnp_properties::NetworkInfo::Starnix(info)) = network.info else {
            return Err(NetworkValidationError::MissingNetworkInfo);
        };
        let starnix_handle = info.handle.ok_or(NetworkValidationError::MissingNetworkInfo)?;

        let mut marks = fnet::Marks::default();
        marks.set_mark(fnet::MARK_DOMAIN_SO_MARK, info.mark);

        let dns_servers =
            NetpolNetworksService::extract_dns_servers(&network.dns_servers, raw_network_id.into());

        Ok(Self {
            network_id,
            delegated: DelegatedNetworkProperties { marks, starnix_handle },
            dns_servers,
            connectivity: network.connectivity,
            name: network.name,
            network_type: network.network_type,
        })
    }
}

/// The result of a delegated network update.
///
/// Returned to the main event loop to propagate system-wide configuration
/// changes (such as DNS server updates) and notify active watchers.
#[derive(Debug, Default, PartialEq)]
pub struct DelegatedNetworkUpdateResult {
    /// If present, contains the new consolidated DNS servers known by the
    /// network registry.
    pub dns_servers: Option<Vec<fnet_name::DnsServer_>>,
}

/// A public wrapper enum for FIDL request streams accepted by
/// [`NetpolNetworksService::add_stream`].
///
/// This type represents incoming streams before they are attached to the service's event loop.
pub enum NetworkRequestStream {
    Networks(fnp_properties::NetworksRequestStream),
    NetworkTokenResolver(fnp_properties::NetworkTokenResolverRequestStream),
    PrivilegedNetworkLookup(fnp_properties::PrivilegedNetworkLookupRequestStream),
    PropertyWatcher {
        connection_id: PropertyWatcherConnectionId,
        stream: fnp_properties::PropertyWatcherRequestStream,
    },
    DelegatedNetworks(fnp_properties::NetworkRegistryRequestStream),
    Reachability(freachability::MonitorRequestStream),
}

impl From<fnp_properties::NetworksRequestStream> for NetworkRequestStream {
    fn from(s: fnp_properties::NetworksRequestStream) -> Self {
        Self::Networks(s)
    }
}
impl From<fnp_properties::NetworkTokenResolverRequestStream> for NetworkRequestStream {
    fn from(s: fnp_properties::NetworkTokenResolverRequestStream) -> Self {
        Self::NetworkTokenResolver(s)
    }
}

impl From<fnp_properties::PrivilegedNetworkLookupRequestStream> for NetworkRequestStream {
    fn from(s: fnp_properties::PrivilegedNetworkLookupRequestStream) -> Self {
        Self::PrivilegedNetworkLookup(s)
    }
}

impl From<fnp_properties::NetworkRegistryRequestStream> for NetworkRequestStream {
    fn from(s: fnp_properties::NetworkRegistryRequestStream) -> Self {
        Self::DelegatedNetworks(s)
    }
}

impl From<freachability::MonitorRequestStream> for NetworkRequestStream {
    fn from(s: freachability::MonitorRequestStream) -> Self {
        Self::Reachability(s)
    }
}

impl From<(PropertyWatcherConnectionId, fnp_properties::PropertyWatcherRequestStream)>
    for NetworkRequestStream
{
    fn from(
        s: (PropertyWatcherConnectionId, fnp_properties::PropertyWatcherRequestStream),
    ) -> Self {
        let (connection_id, stream) = s;
        Self::PropertyWatcher { connection_id, stream }
    }
}

/// A client request stream tagged with its connection id, ending in an epitaph carrying that id.
///
/// The epitaph is what the event loop cleans up on; without it [`futures::stream::SelectAll`]
/// would drop the finished stream silently and the connection's state would leak.
pub(crate) type ConnectionStream<Id, S> = StreamWithEpitaph<Tagged<Id, S>, Id>;

/// Flattens an item from a [`ConnectionStream`] into its connection id and the request, where
/// `None` marks the end of the connection.
fn split_connection_item<Id, T>(item: StreamItem<(Id, T), Id>) -> (Id, Option<T>) {
    match item {
        StreamItem::Item((id, request)) => (id, Some(request)),
        StreamItem::Epitaph(id) => (id, None),
    }
}

/// The single-client `NetworkRegistry` request stream, ending in an epitaph so the
/// event loop observes when the client disconnects.
type DelegatedNetworksStream = StreamWithEpitaph<fnp_properties::NetworkRegistryRequestStream, ()>;

/// An internal wrapper enum for active FIDL request streams stored in
/// [`NetpolNetworksService::streams`].
enum NetworkRequestStreamInner {
    Networks(ConnectionStream<NetworksConnectionId, fnp_properties::NetworksRequestStream>),
    NetworkTokenResolver(fnp_properties::NetworkTokenResolverRequestStream),
    PrivilegedNetworkLookup(fnp_properties::PrivilegedNetworkLookupRequestStream),
    PropertyWatcher(
        ConnectionStream<PropertyWatcherConnectionId, fnp_properties::PropertyWatcherRequestStream>,
    ),
    DelegatedNetworks(DelegatedNetworksStream),
    Reachability(ReachabilityStream),
}

impl futures::Stream for NetworkRequestStreamInner {
    type Item = NetworkRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match *self {
            NetworkRequestStreamInner::Networks(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(split_connection_item)
                        .map(|(id, request)| NetworkAttributesRequest { id, request })
                        .map(NetworkRequest::NetworkAttributes)
                })
            }
            NetworkRequestStreamInner::NetworkTokenResolver(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|request| NetworkTokenResolverRequest { request })
                        .map(NetworkRequest::NetworkTokenResolver)
                })
            }
            NetworkRequestStreamInner::PrivilegedNetworkLookup(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|request| PrivilegedNetworkLookupRequest { request })
                        .map(NetworkRequest::PrivilegedNetworkLookup)
                })
            }
            NetworkRequestStreamInner::PropertyWatcher(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(split_connection_item)
                        .map(|(id, request)| PropertyWatcherRequest { id, request })
                        .map(NetworkRequest::PropertyWatcher)
                })
            }
            NetworkRequestStreamInner::DelegatedNetworks(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|item| match item {
                        StreamItem::Item(request) => {
                            DelegatedNetworksRequest { request: Some(request) }
                        }
                        StreamItem::Epitaph(()) => DelegatedNetworksRequest { request: None },
                    })
                    .map(NetworkRequest::DelegatedNetworks)
                })
            }
            NetworkRequestStreamInner::Reachability(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(split_connection_item)
                        .map(|(id, request)| ReachabilityRequest { id, request })
                        .map(NetworkRequest::Reachability)
                })
            }
        }
    }
}

/// A wrapper for [`fnp_properties::NetworksRequest`] that includes the
/// [`NetworksConnectionId`] of the connection that sent the request, or `None` if the client
/// connection closed.
pub struct NetworkAttributesRequest {
    pub id: NetworksConnectionId,
    pub request: Option<Result<fnp_properties::NetworksRequest, fidl::Error>>,
}

/// A wrapper for [`fnp_properties::NetworkTokenResolverRequest`].
pub struct NetworkTokenResolverRequest {
    pub request: Result<fnp_properties::NetworkTokenResolverRequest, fidl::Error>,
}

/// A wrapper for [`fnp_properties::PrivilegedNetworkLookupRequest`].
pub struct PrivilegedNetworkLookupRequest {
    pub request: Result<fnp_properties::PrivilegedNetworkLookupRequest, fidl::Error>,
}

/// A wrapper for [`fnp_properties::PropertyWatcherRequest`] that includes the
/// [`PropertyWatcherConnectionId`] of the connection that sent the request, or `None` if the
/// client connection closed.
pub struct PropertyWatcherRequest {
    pub id: PropertyWatcherConnectionId,
    pub request: Option<Result<fnp_properties::PropertyWatcherRequest, fidl::Error>>,
}

/// A wrapper for [`fnp_properties::NetworkRegistryRequest`], or `None` if the client
/// connection closed.
pub struct DelegatedNetworksRequest {
    pub request: Option<Result<fnp_properties::NetworkRegistryRequest, fidl::Error>>,
}

/// A wrapper for [`freachability::MonitorRequest`] that includes the
/// [`ReachabilityWatcherConnectionId`] and indicates whether the watcher stream is still open.
pub struct ReachabilityRequest {
    pub id: ReachabilityWatcherConnectionId,
    /// Note: Storing the Request inside the Option allows us to distinguish between the channel
    /// being closed and a Watch request being sent to support cleaner channel-closing logic.
    pub request: Option<Result<freachability::MonitorRequest, fidl::Error>>,
}

/// An enum representing all possible events that can be received by the NetpolNetworksService
/// event loop.
pub enum NetworkRequest {
    NetworkAttributes(NetworkAttributesRequest),
    NetworkTokenResolver(NetworkTokenResolverRequest),
    PrivilegedNetworkLookup(PrivilegedNetworkLookupRequest),
    PropertyWatcher(PropertyWatcherRequest),
    DelegatedNetworks(DelegatedNetworksRequest),
    Reachability(ReachabilityRequest),
}

impl futures::Stream for NetpolNetworksService {
    type Item = NetworkRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.streams.poll_next_unpin(cx)
    }
}

impl futures::stream::FusedStream for NetpolNetworksService {
    fn is_terminated(&self) -> bool {
        self.streams.is_terminated()
    }
}

#[derive(Unit, Debug, Default, Clone)]
struct MethodInspect {
    successes: u32,
    errors: u32,
}

#[derive(Unit, Debug, Default, Clone)]
struct RegistryMetrics {
    adds: MethodInspect,
    removes: MethodInspect,
    /// Counts invocations of the `SetDefault` FIDL method. Both setting a
    /// default network ID and unsetting the default network are
    /// valid operations that count as successes.
    set_defaults: MethodInspect,
    updates: MethodInspect,
}

#[derive(Inspect, Default)]
struct OperationsMetrics {
    delegated: IValue<RegistryMetrics>,
    inspect_node: fuchsia_inspect::Node,
}

impl OperationsMetrics {
    fn record_set_default(
        &mut self,
        result: &Result<(), fnp_properties::NetworkRegistrySetDefaultError>,
    ) {
        let mut delegated = self.delegated.as_mut();
        match result {
            Ok(()) => delegated.set_defaults.successes += 1,
            Err(_) => delegated.set_defaults.errors += 1,
        }
    }

    fn record_add(&mut self, result: &Result<(), fnp_properties::NetworkRegistryAddError>) {
        let mut delegated = self.delegated.as_mut();
        match result {
            Ok(()) => delegated.adds.successes += 1,
            Err(_) => delegated.adds.errors += 1,
        }
    }

    fn record_update(&mut self, result: &Result<(), fnp_properties::NetworkRegistryUpdateError>) {
        let mut delegated = self.delegated.as_mut();
        match result {
            Ok(()) => delegated.updates.successes += 1,
            Err(_) => delegated.updates.errors += 1,
        }
    }

    fn record_remove(&mut self, result: &Result<(), fnp_properties::NetworkRegistryRemoveError>) {
        let mut delegated = self.delegated.as_mut();
        match result {
            Ok(()) => delegated.removes.successes += 1,
            Err(_) => delegated.removes.errors += 1,
        }
    }
}

/// Name of the Inspect property recording how many networks are registered.
const NETWORK_COUNT_PROPERTY_NAME: &str = "network_count";

#[derive(Default)]
pub struct NetpolNetworksService {
    // The current generation
    current_generation: UpdateGeneration,
    // The last generation sent per connection
    generations_by_connection: UpdateGenerations,
    // Networks connections
    networks_clients: HashMap<NetworksConnectionId, NetworksClient>,
    // The active `fuchsia.net.policy.properties.NetworkRegistry` client connection, if any.
    delegated_networks_client: Option<fnp_properties::NetworkRegistryControlHandle>,
    tokens: token_registry::TokenRegistry<NetworkTokenContents>,
    // NetworkProperty Watchers
    property_watchers: HashMap<PropertyWatcherConnectionId, Registration>,
    // The networks known to the system
    network_registry: RegisteredNetworks,
    telemetry: Option<TelemetrySender>,
    // Mints ids for networks connections
    next_networks_id: NetworksConnectionIdAllocator,
    // Mints ids for property watcher connections
    next_watcher_id: PropertyWatcherConnectionIdAllocator,
    // Reachability Monitor Watcher Handler
    reachability_handler: ReachabilityHandler,
    // The multiplexed stream of events handled by the eventloop
    streams: futures::stream::SelectAll<NetworkRequestStreamInner>,

    // Inspect metrics for operations
    metrics: OperationsMetrics,
    // Inspect node for network topology & DNS servers
    networks_inspect_node: Option<fuchsia_inspect::Node>,
    // The number of registered networks. Recorded alongside the network
    // registry node rather than within it so that a registry that holds no
    // networks can be told apart from one whose contents are missing.
    network_count: Option<fuchsia_inspect::UintProperty>,
}

impl NetpolNetworksService {
    pub fn with_operations_inspect(
        mut self,
        parent: &fuchsia_inspect::Node,
        name: impl AsRef<str>,
    ) -> Result<Self, fuchsia_inspect_derive::AttachError> {
        self.metrics = OperationsMetrics::default().with_inspect(parent, name)?;
        Ok(self)
    }

    pub fn with_network_registry_inspect(
        mut self,
        parent: &fuchsia_inspect::Node,
        name: impl AsRef<str>,
    ) -> Self {
        self.networks_inspect_node = Some(parent.create_child(name.as_ref()));
        self.network_count = Some(parent.create_uint(NETWORK_COUNT_PROPERTY_NAME, 0));
        self.update_network_inspect();
        self
    }

    /// Records the current contents of the network registry to Inspect.
    fn update_network_inspect(&self) {
        if let Some(networks_node) = &self.networks_inspect_node {
            self.network_registry.update_inspect(networks_node);
        }
        if let Some(network_count) = &self.network_count {
            network_count.set(self.network_registry.networks.len() as u64);
        }
    }

    pub fn with_inspect(
        self,
        telemetry_parent: &fuchsia_inspect::Node,
        operations_name: impl AsRef<str>,
        networks_parent: &fuchsia_inspect::Node,
        networks_name: impl AsRef<str>,
    ) -> Result<Self, fuchsia_inspect_derive::AttachError> {
        let service = self.with_operations_inspect(telemetry_parent, operations_name)?;
        Ok(service.with_network_registry_inspect(networks_parent, networks_name))
    }

    pub fn synthesize_reachability_snapshot(&self) -> freachability::Snapshot {
        ReachabilityHandler::synthesize_snapshot(
            self.network_registry
                .default_network
                .and_then(|id| self.network_registry.networks.get(&id)),
        )
    }

    fn maybe_notify_watchers(&mut self) {
        let current_snapshot = self.synthesize_reachability_snapshot();
        self.reachability_handler.maybe_notify_watchers(&current_snapshot);
    }

    fn handle_reachability_request(
        &mut self,
        id: ReachabilityWatcherConnectionId,
        request: Option<Result<freachability::MonitorRequest, fidl::Error>>,
    ) {
        let current_snapshot = self.synthesize_reachability_snapshot();
        self.reachability_handler.handle_request(&current_snapshot, id, request);
    }

    #[cfg(test)]
    pub fn reachability_watcher_count(&self) -> usize {
        self.reachability_handler.watcher_count()
    }

    pub fn set_telemetry(&mut self, telemetry: TelemetrySender) {
        self.telemetry = Some(telemetry);
    }

    pub fn add_stream<S: Into<NetworkRequestStream>>(&mut self, s: S) {
        match s.into() {
            NetworkRequestStream::Networks(stream) => {
                let id = self.next_networks_id.allocate();
                let previous = self.networks_clients.insert(
                    id,
                    NetworksClient { responder: None, control_handle: stream.control_handle() },
                );
                assert!(previous.is_none(), "networks client {id:?} is already registered");
                self.streams
                    .push(NetworkRequestStreamInner::Networks(stream.tagged(id).with_epitaph(id)));
            }
            NetworkRequestStream::NetworkTokenResolver(stream) => {
                self.streams.push(NetworkRequestStreamInner::NetworkTokenResolver(stream));
            }
            NetworkRequestStream::PrivilegedNetworkLookup(stream) => {
                self.streams.push(NetworkRequestStreamInner::PrivilegedNetworkLookup(stream));
            }
            NetworkRequestStream::DelegatedNetworks(stream) => {
                if self.delegated_networks_client.is_some() {
                    warn!(
                        "Only one connection to \
                        fuchsia.net.policy.properties/NetworkRegistry is allowed at a time"
                    );
                    stream.control_handle().shutdown_with_epitaph(zx::Status::ALREADY_EXISTS);
                } else {
                    self.delegated_networks_client = Some(stream.control_handle());
                    self.streams.push(NetworkRequestStreamInner::DelegatedNetworks(
                        stream.with_epitaph(()),
                    ));
                }
            }
            NetworkRequestStream::PropertyWatcher { connection_id, stream } => {
                self.streams.push(NetworkRequestStreamInner::PropertyWatcher(
                    stream.tagged(connection_id).with_epitaph(connection_id),
                ));
            }
            NetworkRequestStream::Reachability(stream) => {
                if let Some(reachability_stream) = self.reachability_handler.add_stream(stream) {
                    self.streams.push(NetworkRequestStreamInner::Reachability(reachability_stream));
                }
            }
        }
    }

    pub async fn handle_event(
        &mut self,
        event: NetworkRequest,
    ) -> Result<DelegatedNetworkUpdateResult, anyhow::Error> {
        match event {
            NetworkRequest::NetworkAttributes(NetworkAttributesRequest { id, request }) => {
                self.handle_network_attributes_request(id, request).await?;
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
            NetworkRequest::NetworkTokenResolver(NetworkTokenResolverRequest { request }) => {
                self.handle_network_token_resolver_request(request).await?;
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
            NetworkRequest::PrivilegedNetworkLookup(PrivilegedNetworkLookupRequest { request }) => {
                self.handle_privileged_network_lookup_request(request)?;
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
            NetworkRequest::DelegatedNetworks(DelegatedNetworksRequest { request }) => {
                Ok(self.handle_delegated_networks_update(request).await)
            }
            NetworkRequest::PropertyWatcher(PropertyWatcherRequest { id, request }) => {
                self.handle_property_watcher_request(id, request).await?;
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
            NetworkRequest::Reachability(ReachabilityRequest { id, request }) => {
                self.handle_reachability_request(id, request);
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
        }
    }

    /// Returns the consolidated DNS servers from the Network Registry.
    pub fn consolidated_dns_servers(&self) -> Vec<fnet_name::DnsServer_> {
        self.network_registry.consolidated_dns_servers()
    }

    /// Handles a single item produced by a client's [`NetworksRequestStream`].
    ///
    /// # Panics
    ///
    /// Panics if `id` does not correspond to a currently registered client. A client is removed
    /// only when its stream yields the terminal `None` item, and the stream is fused, so every
    /// item from the stream belongs to a live client.
    pub async fn handle_network_attributes_request(
        &mut self,
        id: NetworksConnectionId,
        req: Option<Result<fnp_properties::NetworksRequest, fidl::Error>>,
    ) -> Result<(), anyhow::Error> {
        let mut entry = assert_matches!(
            self.networks_clients.entry(id),
            Entry::Occupied(entry) => entry,
            "request for unknown networks client {id:?}"
        );

        let req = match req {
            Some(Ok(req)) => req,
            Some(Err(e)) => {
                // A clean disconnect yields the terminal item below rather than an error, so
                // this is always abnormal: a malformed request or a channel-level read failure.
                // No epitaph is sent because the channel may no longer be writable.
                error!("Networks client {id:?} stream error: {e}");
                entry.get_mut().close(None);
                return Ok(());
            }
            // The client's stream has terminated, either because the client went away or
            // because of a `close` above. This is the only place clients are removed.
            None => {
                let _: NetworksClient = entry.remove();
                let _: Option<_> = self.generations_by_connection.remove_default(&id);
                return Ok(());
            }
        };
        match req {
            fnp_properties::NetworksRequest::WatchDefault { responder } => {
                let client = entry.get_mut();
                if client.responder.is_some() {
                    warn!(
                        "Only one call to fuchsia.net.policy.properties/Networks.WatchDefault \
                          may be active per connection"
                    );
                    // Close the connection; the terminal item performs the cleanup.
                    client.close(Some(zx::Status::ALREADY_EXISTS));
                } else {
                    let network_id =
                        if self.generations_by_connection.default_network(&id).unwrap_or_default()
                            < self.current_generation.default_network
                        {
                            self.network_registry.default_network
                        } else {
                            None
                        };
                    if let Some(network_id) = network_id {
                        self.generations_by_connection
                            .set_default_network(id, self.current_generation);
                        let token = self
                            .tokens
                            .ensure_token(NetworkTokenContents { network_id, is_default: true })
                            .get()
                            .duplicate()
                            .context("could not duplicate token")?;
                        responder
                            .send(fnp_properties::NetworksWatchDefaultResponse::Network(token))?;
                    } else {
                        client.responder = Some(responder);
                    }
                }
            }
            fnp_properties::NetworksRequest::WatchProperties {
                payload:
                    fnp_properties::NetworksWatchPropertiesRequest {
                        network, properties, watcher, ..
                    },
                responder,
            } => match (network, properties, watcher) {
                (None, _, _) | (_, None, _) | (_, _, None) => {
                    responder.send(Err(fnp_properties::WatchError::MissingRequiredArgument))?
                }
                (Some(network), Some(properties), Some(watcher)) => {
                    if properties == fnp_properties::PropertyInterest::default() {
                        responder.send(Err(fnp_properties::WatchError::NoProperties))?
                    } else {
                        match self.tokens.get_contents(&network) {
                            Err(e) => {
                                warn!("Unknown network token. ({network:?}: {e})");
                                responder
                                    .send(Err(fnp_properties::WatchError::InvalidNetworkToken))?
                            }
                            Ok(_network_contents) => {
                                // Bind the stateful PropertyWatcher session: cache the token and
                                // requested properties, register the watcher's event stream, and
                                // reply immediately.
                                let watcher_stream = watcher.into_stream();
                                let watcher_id = self.next_watcher_id.allocate();
                                let registration = Registration {
                                    token: network,
                                    properties,
                                    responder: None,
                                    control_handle: watcher_stream.control_handle(),
                                };
                                let previous =
                                    self.property_watchers.insert(watcher_id, registration);
                                assert!(
                                    previous.is_none(),
                                    "property watcher {watcher_id:?} is already registered"
                                );
                                self.add_stream((watcher_id, watcher_stream));
                                responder.send(Ok(()))?;
                            }
                        }
                    }
                }
            },
            fnp_properties::NetworksRequest::_UnknownMethod { ordinal, .. } => {
                warn!("Received unexpected request {ordinal}")
            }
        }

        Ok(())
    }

    /// Handles a single item produced by a [`PrivilegedNetworkLookupRequestStream`].
    fn handle_privileged_network_lookup_request(
        &mut self,
        req: Result<fnp_properties::PrivilegedNetworkLookupRequest, fidl::Error>,
    ) -> Result<(), anyhow::Error> {
        let req = req.context("while handling PrivilegedNetworkLookup request")?;
        match req {
            fnp_properties::PrivilegedNetworkLookupRequest::Lookup { payload, responder } => {
                let result = match payload {
                    fnp_properties::LookupRequest::StarnixHandle(starnix_handle) => {
                        // Each delegated network registered by Starnix has a unique handle, so at
                        // most one entry in `networks` will match `starnix_handle`. The returned
                        // token is keyed by `network_id` rather than `starnix_handle`, so it
                        // remains active even if `starnix_handle` is updated later, until the
                        // network itself is removed.
                        match self.network_registry.networks.iter().find_map(
                            |(&network_id, props)| {
                                props
                                    .delegated
                                    .as_ref()
                                    .is_some_and(|d| d.starnix_handle == starnix_handle)
                                    .then_some(network_id)
                            },
                        ) {
                            Some(network_id) => Ok(self
                                .tokens
                                .ensure_token(NetworkTokenContents {
                                    network_id,
                                    is_default: false,
                                })
                                .get()
                                .duplicate()
                                .context("could not duplicate token")?),
                            None => Err(fnp_properties::LookupError::NotFound),
                        }
                    }
                    fnp_properties::LookupRequest::__SourceBreaking { unknown_ordinal } => {
                        warn!(
                            "Received unknown LookupRequest union variant in \
                            PrivilegedNetworkLookup.Lookup: {unknown_ordinal}"
                        );
                        Err(fnp_properties::LookupError::NotFound)
                    }
                };
                if let Err(e) = responder.send(result) {
                    warn!("Could not send PrivilegedNetworkLookup.Lookup response: {e}");
                }
            }
            fnp_properties::PrivilegedNetworkLookupRequest::_UnknownMethod { ordinal, .. } => {
                warn!(
                    "Received unexpected fuchsia.net.policy.properties/PrivilegedNetworkLookup \
                    request {ordinal}"
                );
            }
        }
        Ok(())
    }

    /// Handles a single item produced by a client's [`PropertyWatcherRequestStream`].
    ///
    /// # Panics
    ///
    /// Panics if `id` does not correspond to a currently registered client. A client is removed
    /// only when its stream yields the terminal `None` item, and the stream is fused, so every
    /// item from the stream belongs to a live client.
    pub async fn handle_property_watcher_request(
        &mut self,
        id: PropertyWatcherConnectionId,
        req: Option<Result<fnp_properties::PropertyWatcherRequest, fidl::Error>>,
    ) -> Result<(), anyhow::Error> {
        let mut entry = assert_matches!(
            self.property_watchers.entry(id),
            Entry::Occupied(entry) => entry,
            "request for unknown property watcher {id:?}"
        );

        let req = match req {
            Some(Ok(req)) => req,
            Some(Err(e)) => {
                // A clean disconnect yields the terminal item below rather than an error, so
                // this is always abnormal: a malformed request or a channel-level read failure.
                // No epitaph is sent because the channel may no longer be writable.
                error!("Property watcher {id:?} stream error: {e}");
                entry.get_mut().close(None);
                return Ok(());
            }
            // The client's stream has terminated, either because the client went away or
            // because of a `close` above. This is the only place registrations are removed.
            None => {
                let _: Registration = entry.remove();
                let _: Option<_> = self.generations_by_connection.remove_properties(&id);
                return Ok(());
            }
        };
        match req {
            fnp_properties::PropertyWatcherRequest::Watch { responder } => {
                let registration = entry.get_mut();
                if registration.responder.is_some() {
                    warn!(
                        "Only one call to \
                        fuchsia.net.policy.properties/PropertyWatcher.Watch may be \
                        active per connection"
                    );
                    // Close the connection; the terminal item performs the cleanup.
                    registration.close(Some(zx::Status::ALREADY_EXISTS));
                } else {
                    registration.responder = Some(responder);
                    match self.tokens.get_contents(&registration.token) {
                        Ok(network_contents) => {
                            // Determine whether a new update is available
                            // (last_sent_generation < current_generation)
                            let is_initial =
                                self.generations_by_connection.properties(&id).is_none();
                            if is_initial
                                || self
                                    .generations_by_connection
                                    .properties(&id)
                                    .unwrap_or_default()
                                    < self.current_generation.properties
                            {
                                let mut updates = fnp_properties::PropertyUpdate::default();
                                updates.add_socket_marks(
                                    &self.network_registry,
                                    &network_contents,
                                    registration.properties,
                                );
                                updates.add_dns(
                                    &self.network_registry,
                                    &network_contents,
                                    registration.properties,
                                );
                                let network_is_known = self
                                    .network_registry
                                    .networks
                                    .contains_key(&network_contents.network_id);
                                if (is_initial && network_is_known)
                                    || updates != fnp_properties::PropertyUpdate::default()
                                {
                                    self.generations_by_connection
                                        .set_properties(id, self.current_generation);
                                    if let Some(responder) = registration.responder.take() {
                                        responder.send(Ok(&updates))?;
                                    }
                                }
                            }
                        }
                        // `NOT_FOUND` means the network was removed, and its token dropped,
                        // before this `Watch` call. Any other error is unexpected.
                        Err(e) => {
                            if e != zx::Status::NOT_FOUND {
                                warn!("Unexpected error fetching token contents: {e}");
                            }
                            let _: Option<_> =
                                self.generations_by_connection.remove_properties(&id);
                            if let Some(responder) = registration.responder.take() {
                                if let Err(e) = responder
                                    .send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                                {
                                    warn!("Could not send to responder: {e}");
                                }
                                registration.close(None);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Handles delegated network updates coming from Starnix.
    ///
    /// Resolves network events requested through the `NetworkRegistry` interface, applies the
    /// corresponding properties changes, and yields the computed DNS configuration targets for
    /// output updates.
    ///
    /// TODO(https://fxbug.dev/428712735): Stop returning DnsServer list once
    /// dns-resolver learns about DNS via NetworkProperties.
    ///
    /// # Panics
    ///
    /// Panics if there is no currently active delegated networks client. A client is removed
    /// only when its stream yields the terminal `None` item, and the stream is fused, so every
    /// item from the stream belongs to a live client.
    pub async fn handle_delegated_networks_update(
        &mut self,
        update: Option<Result<fnp_properties::NetworkRegistryRequest, fidl::Error>>,
    ) -> DelegatedNetworkUpdateResult {
        use fnp_properties::{
            NetworkRegistryAddError, NetworkRegistryRemoveError, NetworkRegistryRequest,
            NetworkRegistrySetDefaultError, NetworkRegistryUpdateError,
        };

        let control_handle = assert_matches!(
            self.delegated_networks_client.as_ref(),
            Some(control_handle) => control_handle,
            "request with no active delegated networks client"
        );

        let req = match update {
            Some(Ok(req)) => req,
            Some(Err(e)) => {
                // A clean disconnect yields the terminal item below rather than an error, so
                // this is always abnormal: a malformed request or a channel-level read failure.
                // No epitaph is sent because the channel may no longer be writable.
                error!("Delegated networks client stream error: {e}");
                control_handle.shutdown();
                return DelegatedNetworkUpdateResult { dns_servers: None };
            }
            // The client's stream has terminated, either because the client went away or
            // because of a `shutdown` above. This is the only place the client is removed.
            None => {
                let _: Option<_> = self.delegated_networks_client.take();
                return self.clear_delegated_networks().await;
            }
        };

        let action_result = match req {
            NetworkRegistryRequest::SetDefault { network_id, responder } => {
                let set_default_result = match network_id {
                    fposix_socket::OptionalUint32::Value(interface_id) => {
                        match InterfaceId::try_from(interface_id) {
                            Ok(id) => {
                                let delegated_id = NetworkId::delegated(id);
                                if self.network_registry.networks.contains_key(&delegated_id) {
                                    self.update(NetworkRegistryUpdate::ChangeNetwork(
                                        delegated_id,
                                        NetworkUpdate::MakeDefault,
                                    ))
                                    .await;
                                    Ok(())
                                } else {
                                    Err(NetworkRegistrySetDefaultError::NotFound)
                                }
                            }
                            Err(_) => Err(NetworkRegistrySetDefaultError::NotFound),
                        }
                    }
                    fposix_socket::OptionalUint32::Unset(_) => {
                        self.update(NetworkRegistryUpdate::unset_default()).await;
                        Ok(())
                    }
                };

                self.metrics.record_set_default(&set_default_result);

                self.respond_to_delegated_network_update(
                    set_default_result,
                    |reply| responder.send(reply),
                    "failed to send SetDefault result",
                )
            }
            NetworkRegistryRequest::Add { network, responder } => {
                let add_result = match ValidatedNetwork::try_from(network).map_err(Into::into) {
                    Ok(valid) => {
                        if self.network_registry.networks.contains_key(&valid.network_id) {
                            Err(NetworkRegistryAddError::DuplicateNetworkId)
                        } else {
                            self.update(NetworkRegistryUpdate::ChangeNetwork(
                                valid.network_id,
                                NetworkUpdate::Properties(NetworkPropertiesChange {
                                    added: true,
                                    delegated: Some(valid.delegated),
                                    dns_servers: Some(valid.dns_servers),
                                    connectivity_state: valid.connectivity,
                                    name: valid.name,
                                    network_type: valid.network_type,
                                }),
                            ))
                            .await;
                            Ok(())
                        }
                    }
                    Err(e) => Err(e),
                };

                self.metrics.record_add(&add_result);

                self.respond_to_delegated_network_update(
                    add_result,
                    |reply| responder.send(reply),
                    "failed to send Add result",
                )
            }
            NetworkRegistryRequest::Update { network, responder } => {
                let update_result = match ValidatedNetwork::try_from(network).map_err(Into::into) {
                    Ok(valid) => {
                        if !self.network_registry.networks.contains_key(&valid.network_id) {
                            Err(NetworkRegistryUpdateError::NotFound)
                        } else {
                            self.update(NetworkRegistryUpdate::ChangeNetwork(
                                valid.network_id,
                                NetworkUpdate::Properties(NetworkPropertiesChange {
                                    added: false,
                                    delegated: Some(valid.delegated),
                                    dns_servers: Some(valid.dns_servers),
                                    connectivity_state: valid.connectivity,
                                    name: valid.name,
                                    network_type: valid.network_type,
                                }),
                            ))
                            .await;
                            Ok(())
                        }
                    }
                    Err(e) => Err(e),
                };

                self.metrics.record_update(&update_result);

                self.respond_to_delegated_network_update(
                    update_result,
                    |reply| responder.send(reply),
                    "failed to send Update result",
                )
            }
            NetworkRegistryRequest::Remove { network_id, responder } => {
                let remove_result = match InterfaceId::try_from(network_id) {
                    Ok(id) => {
                        let delegated_id = NetworkId::delegated(id);
                        if self.network_registry.starnix_default == Some(delegated_id) {
                            Err(NetworkRegistryRemoveError::CannotRemoveDefaultNetwork)
                        } else if !self.network_registry.networks.contains_key(&delegated_id) {
                            Err(NetworkRegistryRemoveError::NotFound)
                        } else {
                            self.update(NetworkRegistryUpdate::ChangeNetwork(
                                delegated_id,
                                NetworkUpdate::Remove,
                            ))
                            .await;
                            Ok(())
                        }
                    }
                    Err(_) => Err(NetworkRegistryRemoveError::NotFound),
                };

                self.metrics.record_remove(&remove_result);

                self.respond_to_delegated_network_update(
                    remove_result,
                    |reply| responder.send(reply),
                    "failed to send Remove result",
                )
            }
        };

        action_result
    }

    /// Unsets `starnix_default` and removes all `NetworkId::Delegated` networks when the
    /// `NetworkRegistry` client disconnects, returning the updated DNS server list if any
    /// delegated networks were removed.
    async fn clear_delegated_networks(&mut self) -> DelegatedNetworkUpdateResult {
        // Unset `starnix_default` first because `RegisteredNetworks::apply` refuses to remove a
        // network while it is still marked as `starnix_default`.
        if self.network_registry.starnix_default.is_some() {
            self.update(NetworkRegistryUpdate::unset_default()).await;
        }

        let delegated_ids = self
            .network_registry
            .networks
            .keys()
            .filter(|id| match id {
                NetworkId::Delegated(_) => true,
                NetworkId::Fuchsia(_) => false,
            })
            .copied()
            .collect::<Vec<_>>();

        for id in &delegated_ids {
            self.update(NetworkRegistryUpdate::ChangeNetwork(*id, NetworkUpdate::Remove)).await;
        }

        DelegatedNetworkUpdateResult {
            dns_servers: (!delegated_ids.is_empty())
                .then(|| self.network_registry.consolidated_dns_servers()),
        }
    }

    // Resolves the operation result, sends the success or failure status to
    // the FIDL responder, and returns the updated network registry settings.
    fn respond_to_delegated_network_update<E, F>(
        &self,
        operation_result: Result<(), E>,
        send_response: F,
        context_message: &'static str,
    ) -> DelegatedNetworkUpdateResult
    where
        F: FnOnce(Result<(), E>) -> Result<(), fidl::Error>,
    {
        // Return consolidated DNS servers if the operation was successful.
        let dns_servers = operation_result
            .as_ref()
            .ok()
            .map(|()| self.network_registry.consolidated_dns_servers());

        // Send success or failure status to the FIDL responder.
        if let Err(e) = send_response(operation_result) {
            if !e.is_closed() {
                error!(
                    "Failed to send delegated network update result \
                for {context_message}: {e}"
                );
            }
        }

        DelegatedNetworkUpdateResult { dns_servers }
    }

    // Converts `NetworkDnsServers` to `Vec<DnsServer_>` for a given network.
    //
    // Note: We prioritize IPv4 servers over IPv6 servers. This is impactful
    // when sending DNS servers through NetworkProperties or to dns-resolver.
    fn extract_dns_servers(
        dns_servers: &Option<fnp_properties::NetworkDnsServers>,
        network_id: u64,
    ) -> Vec<fnet_name::DnsServer_> {
        let make_server = |address| fnet_name::DnsServer_ {
            address: Some(address),
            source: Some(fnet_name::DnsServerSource::SocketProxy(
                fnet_name::SocketProxyDnsServerSource {
                    source_interface: Some(network_id),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        dns_servers
            .as_ref()
            .map(|dns| {
                dns.v4
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .map(|&address| {
                        make_server(fnet::SocketAddress::Ipv4(fnet::Ipv4SocketAddress {
                            address,
                            port: DNS_PORT,
                        }))
                    })
                    .chain(dns.v6.as_ref().into_iter().flatten().map(|&address| {
                        make_server(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address,
                            port: DNS_PORT,
                            zone_index: 0,
                        }))
                    }))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn handle_network_token_resolver_request(
        &mut self,
        request: Result<fnp_properties::NetworkTokenResolverRequest, fidl::Error>,
    ) -> Result<(), anyhow::Error> {
        use fnp_properties::NetworkTokenResolverResolveTokenError as ResolveTokenError;

        let request = request.context("while handling NetworkTokenResolver request")?;
        match request {
            fnp_properties::NetworkTokenResolverRequest::ResolveToken { token, responder } => {
                let maybe_contents = self.tokens.get_contents(&token).copied();
                match maybe_contents {
                    Err(e) => {
                        warn!("Unknown network token. ({token:?}: {e})");
                        responder.send(Err(ResolveTokenError::InvalidNetworkToken))?;
                    }
                    Ok(contents) => {
                        if contents.is_default {
                            // This is a default network token, we need to grab
                            // the non-default variant.
                            let query = NetworkTokenContents { is_default: false, ..contents };
                            if let Some(tok) = self.tokens.get_token(&query) {
                                responder.send(tok.duplicate().map_err(|e| {
                                    warn!("Encountered issue duplicating generated token. {e}");
                                    ResolveTokenError::InvalidNetworkToken
                                }))?;
                            } else {
                                warn!("Requested canonical version of unregistered network.");
                                responder.send(Err(ResolveTokenError::InvalidNetworkToken))?;
                            }
                        } else {
                            responder.send(Ok(token))?;
                        }
                    }
                }
            }
            fidl_fuchsia_net_policy_properties::NetworkTokenResolverRequest::_UnknownMethod {
                ordinal,
                control_handle,
                method_type,
                ..
            } => warn!(
                "Encountered unknown method call on NetworkTokenResolver: {ordinal} \
                {control_handle:?} {method_type:?}"
            ),
        }

        Ok(())
    }

    async fn changed_default_network(
        &mut self,
        previous_default_network: Option<NetworkId>,
        property_watchers: &mut HashMap<PropertyWatcherConnectionId, Registration>,
    ) {
        for (id, registration) in property_watchers.iter_mut() {
            if let Ok(contents) = self.tokens.get_contents(&registration.token) {
                if contents.is_default {
                    let _: Option<_> = self.generations_by_connection.remove_properties(id);
                    if let Some(responder) = registration.responder.take() {
                        if let Err(e) =
                            responder.send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                        {
                            warn!("Could not send to responder: {e}");
                        }
                        registration.close(None);
                    }
                }
            }
        }
        self.tokens.drop_if(|&c| {
            c.is_default && previous_default_network.is_some_and(|i| i == c.network_id)
        });
    }

    pub async fn update(&mut self, update: NetworkRegistryUpdate) {
        let RegistryUpdateResult { event, default_changed } = self.network_registry.apply(update);

        self.update_network_inspect();

        if let UpdateApplied::None = event {
            if default_changed.is_none() {
                // Return early if there were absolutely no changes and the default stayed the same.
                return;
            }
        }

        if event != UpdateApplied::None {
            self.current_generation.properties += 1;
        }

        let mut property_watchers = HashMap::new();
        std::mem::swap(&mut self.property_watchers, &mut property_watchers);

        // Clean up or register tokens based on whether the network was added or removed.
        match event {
            UpdateApplied::NetworkChanged { network_id, added: true, .. } => {
                let _ = self
                    .tokens
                    .ensure_token(NetworkTokenContents { network_id, is_default: false });
            }
            UpdateApplied::NetworkRemoved(network_id) => {
                info!("Removing network {network_id}. Reporting NETWORK_GONE to watchers.");
                // Notify all property watchers bound to the removed network. The watcher is
                // kept in the `property_watchers` map so that idle clients
                // can receive NETWORK_GONE.
                for (id, registration) in property_watchers.iter_mut() {
                    if let Ok(network) = self.tokens.get_contents(&registration.token) {
                        if network.network_id == network_id {
                            let _: Option<_> = self.generations_by_connection.remove_properties(id);
                            if let Some(responder) = registration.responder.take() {
                                if let Err(e) = responder
                                    .send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                                {
                                    warn!("Could not send to responder: {e}");
                                }
                                registration.close(None);
                            }
                        }
                    }
                }
                self.tokens.drop_if(|c| !c.is_default && c.network_id == network_id);
            }
            UpdateApplied::NetworkChanged { added: false, .. }
            | UpdateApplied::DnsChanged
            | UpdateApplied::None => {}
        }

        // Notify watchers of default network changes if one occurred.
        if let Some(DefaultChangedEvent { previous_default }) = default_changed {
            self.notify_default_network_changed(previous_default, &mut property_watchers).await;
            std::mem::swap(&mut self.property_watchers, &mut property_watchers);
            self.maybe_notify_watchers();
            return;
        }

        if let UpdateApplied::NetworkChanged { network_id, .. } = event {
            if let Some(telemetry) = &self.telemetry {
                if let Some(props) = self.network_registry.networks.get(&network_id) {
                    telemetry.send(TelemetryEvent::NetworkChanged(NetworkEventMetadata {
                        id: network_id.get().get(),
                        name: props.name.clone(),
                        transport: props
                            .network_type
                            .unwrap_or(fnp_properties::NetworkType::Unknown),
                        is_fuchsia_provisioned: network_id.is_fuchsia(),
                        connectivity_state: props.connectivity_state,
                    }));
                }
            }
        }

        for (id, mut registration) in property_watchers {
            let mut updates = fnp_properties::PropertyUpdate::default();
            match self.tokens.get_contents(&registration.token) {
                Ok(network) => match event {
                    UpdateApplied::NetworkChanged {
                        network_id,
                        changed_marks,
                        changed_dns,
                        ..
                    } => {
                        if network.network_id == network_id {
                            if changed_marks {
                                updates.add_socket_marks(
                                    &self.network_registry,
                                    &network,
                                    registration.properties,
                                );
                            }
                            if changed_dns {
                                updates.add_dns(
                                    &self.network_registry,
                                    &network,
                                    registration.properties,
                                );
                            }
                        }
                    }
                    UpdateApplied::DnsChanged => {
                        updates.add_dns(&self.network_registry, &network, registration.properties);
                    }
                    UpdateApplied::NetworkRemoved(_id) => {}
                    UpdateApplied::None => {}
                },
                Err(e) => {
                    debug!(
                        "Token {:?} not found for watcher {:?};
                    network was likely removed while idle ({})",
                        registration.token, id, e
                    );
                }
            }

            // Update the client's generation state to keep them in sync with the global
            // properties generation.
            let has_updates = updates != fnp_properties::PropertyUpdate::default();
            if self.generations_by_connection.properties(&id).is_some() {
                match (has_updates, registration.responder.take()) {
                    (true, Some(responder)) => {
                        // Sync generation and send update.
                        self.generations_by_connection.set_properties(id, self.current_generation);
                        if let Err(e) = responder.send(Ok(&updates)) {
                            warn!("Failed to send watch updates: {}", e);
                        }
                    }
                    (false, maybe_responder) => {
                        // Sync generation to catch up and restore responder.
                        self.generations_by_connection.set_properties(id, self.current_generation);
                        registration.responder = maybe_responder;
                    }
                    (true, None) => {
                        // If a relevant change occurs while the client is idle, we leave the client
                        // on the old generation so they receive the update immediately upon the next
                        // `Watch()` call.
                    }
                }
            }

            assert_matches!(
                self.property_watchers.insert(id, registration),
                None,
                "Re-inserted in an existing registration slot."
            );
        }
        self.maybe_notify_watchers();
    }

    async fn notify_default_network_changed(
        &mut self,
        old_default: Option<NetworkId>,
        property_watchers: &mut HashMap<PropertyWatcherConnectionId, Registration>,
    ) {
        self.changed_default_network(old_default, property_watchers).await;
        let default_network = self.network_registry.default_network;
        if let Some(telemetry) = &self.telemetry {
            match default_network {
                Some(default_network) => {
                    if let Some(props) = self.network_registry.networks.get(&default_network) {
                        telemetry.send(TelemetryEvent::DefaultNetworkChanged(
                            NetworkEventMetadata {
                                id: default_network.get().get(),
                                name: props.name.clone(),
                                transport: props
                                    .network_type
                                    .unwrap_or(fnp_properties::NetworkType::Unknown),
                                is_fuchsia_provisioned: default_network.is_fuchsia(),
                                connectivity_state: props.connectivity_state,
                            },
                        ));
                    } else {
                        warn!("Could not fetch network data for default network.");
                    }
                }
                None => telemetry.send(TelemetryEvent::DefaultNetworkLost),
            }
        }
        self.current_generation.default_network += 1;
        // Answer every outstanding `WatchDefault`. Only the responders are taken; the clients
        // stay registered until their streams end.
        let responders = self
            .networks_clients
            .iter_mut()
            .filter_map(|(id, client)| client.responder.take().map(|responder| (*id, responder)));
        for (id, responder) in responders {
            self.generations_by_connection.set_default_network(id, self.current_generation);
            let response = match default_network {
                Some(network_id) => {
                    match self
                        .tokens
                        .ensure_token(NetworkTokenContents { network_id, is_default: true })
                        .get()
                        .duplicate()
                    {
                        Ok(token) => fnp_properties::NetworksWatchDefaultResponse::Network(token),
                        Err(e) => {
                            warn!("Could not duplicate token: {e}");
                            continue;
                        }
                    }
                }
                None => fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(
                    fnp_properties::Empty,
                ),
            };
            if let Err(e) = responder.send(response) {
                warn!("Could not send to responder: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fnp_properties::{
        NetworkInfo, NetworkRegistryAddError, NetworkRegistryMarker, NetworkRegistryRemoveError,
        NetworkRegistrySetDefaultError, NetworkRegistryUpdateError, StarnixNetworkInfo,
    };
    use futures::FutureExt as _;
    use std::num::NonZeroU64;
    use test_case::test_case;

    const ID_1: InterfaceId = InterfaceId(NonZeroU64::new(1).unwrap());
    const ID_2: InterfaceId = InterfaceId(NonZeroU64::new(2).unwrap());
    const NAME_1: &str = "testif1";
    const NAME_2: &str = "testif2";

    const FUCHSIA_ID_1: NetworkId = NetworkId::Fuchsia(ID_1);
    const FUCHSIA_ID_2: NetworkId = NetworkId::Fuchsia(ID_2);
    const DELEGATED_ID_1: NetworkId = NetworkId::Delegated(ID_1);
    const DELEGATED_ID_2: NetworkId = NetworkId::Delegated(ID_2);

    #[derive(Clone, Copy)]
    struct TestNetwork {
        id: NetworkId,
        name: &'static str,
        network_type: fnp_properties::NetworkType,
    }

    #[derive(Default)]
    struct AppliedChanges {
        added: bool,
        changed_marks: bool,
        changed_dns: bool,
    }

    impl TestNetwork {
        fn added(&self, added: bool) -> NetworkPropertiesChange {
            NetworkPropertiesChange {
                added,
                name: Some(self.name.to_string()),
                network_type: Some(self.network_type),
                ..Default::default()
            }
        }

        fn change_with(
            &self,
            added: bool,
            marks: Option<fnet::Marks>,
            dns_servers: Option<Vec<fnet_name::DnsServer_>>,
            connectivity_state: Option<fnp_properties::ConnectivityState>,
        ) -> NetworkPropertiesChange {
            NetworkPropertiesChange {
                added,
                delegated: marks
                    .map(|marks| DelegatedNetworkProperties { marks, ..Default::default() }),
                dns_servers,
                connectivity_state,
                name: Some(self.name.to_string()),
                network_type: Some(self.network_type),
            }
        }

        fn applied(&self, changes: AppliedChanges) -> UpdateApplied {
            let AppliedChanges { added, changed_marks, changed_dns } = changes;
            UpdateApplied::NetworkChanged {
                network_id: self.id,
                added,
                changed_marks,
                changed_dns,
                name: Some(self.name.to_string()),
                network_type: Some(self.network_type),
            }
        }
    }

    const DELEGATED_NET_1: TestNetwork = TestNetwork {
        id: DELEGATED_ID_1,
        name: NAME_1,
        network_type: fnp_properties::NetworkType::Ethernet,
    };
    const FUCHSIA_NET_1: TestNetwork = TestNetwork {
        id: FUCHSIA_ID_1,
        name: NAME_1,
        network_type: fnp_properties::NetworkType::Ethernet,
    };
    const FUCHSIA_NET_2: TestNetwork = TestNetwork {
        id: FUCHSIA_ID_2,
        name: NAME_2,
        network_type: fnp_properties::NetworkType::Wifi,
    };

    fn test_marks() -> fnet::Marks {
        fnet::Marks { mark_1: Some(123), ..Default::default() }
    }

    fn test_marks_updated() -> fnet::Marks {
        fnet::Marks { mark_1: Some(456), ..Default::default() }
    }

    fn test_dns_1() -> Vec<fnet_name::DnsServer_> {
        vec![fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("192.0.2.1:53")),
            ..Default::default()
        }]
    }

    fn test_dns_2() -> Vec<fnet_name::DnsServer_> {
        vec![fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("192.0.2.2:53")),
            ..Default::default()
        }]
    }

    fn delegated_properties() -> NetworkProperties {
        NetworkProperties {
            delegated: Some(DelegatedNetworkProperties {
                marks: test_marks(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn fuchsia_properties() -> NetworkProperties {
        NetworkProperties::default()
    }

    fn added_properties(name: &str) -> NetworkPropertiesChange {
        NetworkPropertiesChange {
            added: true,
            delegated: None,
            dns_servers: None,
            connectivity_state: None,
            name: Some(name.to_string()),
            network_type: Some(fnp_properties::NetworkType::Ethernet),
        }
    }

    impl NetpolNetworksService {
        pub(crate) fn default_network(&self) -> Option<NetworkId> {
            self.network_registry.default_network
        }

        pub(crate) fn has_network(&self, id: NetworkId) -> bool {
            self.network_registry.networks.contains_key(&id)
        }

        pub(crate) fn has_token(&self, network_id: NetworkId, is_default: bool) -> bool {
            self.tokens.get_token(&NetworkTokenContents { network_id, is_default }).is_some()
        }

        pub(crate) fn ensure_token_for_test(&mut self, network_id: NetworkId, is_default: bool) {
            let _token: crate::network::token_registry::TokenEntry<'_, _> =
                self.tokens.ensure_token(NetworkTokenContents { network_id, is_default });
        }
    }

    #[test_case(
        DELEGATED_NET_1,
        Some(test_marks()),
        Some(test_marks_updated()); "delegated network"
    )]
    #[test_case(FUCHSIA_NET_2, None, None; "fuchsia network")]
    fn test_handle_changed_network(
        net: TestNetwork,
        initial_marks: Option<fnet::Marks>,
        updated_marks: Option<fnet::Marks>,
    ) {
        let mut networks = RegisteredNetworks::default();
        let dns1 = test_dns_1();
        let dns2 = test_dns_2();

        // Adding network with DNS servers should have changed_marks=true and changed_dns=true.
        let event = net.change_with(
            true,
            initial_marks.clone(),
            Some(dns1.clone()),
            Some(fnp_properties::ConnectivityState::FullConnectivity),
        );
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: true, changed_marks: true, changed_dns: true }),
        );
        assert_eq!(
            networks.networks.get(&net.id).expect("network should be present"),
            &NetworkProperties {
                delegated: initial_marks
                    .clone()
                    .map(|marks| DelegatedNetworkProperties { marks, ..Default::default() }),
                dns_servers: dns1,
                connectivity_state: Some(fnp_properties::ConnectivityState::FullConnectivity),
                name: Some(net.name.to_string()),
                network_type: Some(net.network_type),
            }
        );

        // Updating with same marks and different DNS should have changed_marks=false and
        // changed_dns=true.
        let event = net.change_with(
            false,
            initial_marks.clone(),
            Some(dns2.clone()),
            Some(fnp_properties::ConnectivityState::NoConnectivity),
        );
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: false, changed_marks: false, changed_dns: true }),
        );
        assert_eq!(
            networks.networks.get(&net.id).expect("network should be present"),
            &NetworkProperties {
                delegated: initial_marks
                    .clone()
                    .map(|marks| DelegatedNetworkProperties { marks, ..Default::default() }),
                dns_servers: dns2.clone(),
                connectivity_state: Some(fnp_properties::ConnectivityState::NoConnectivity),
                name: Some(net.name.to_string()),
                network_type: Some(net.network_type),
            }
        );

        // Updating marks (if delegated) with same DNS should have changed_dns=false.
        let marks_changed = initial_marks != updated_marks;
        let event = net.change_with(
            false,
            updated_marks.clone(),
            Some(dns2.clone()),
            Some(fnp_properties::ConnectivityState::NoConnectivity),
        );
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges {
                added: false,
                changed_marks: marks_changed,
                changed_dns: false,
            }),
        );
        assert_eq!(
            networks.networks.get(&net.id).expect("network should be present"),
            &NetworkProperties {
                delegated: updated_marks
                    .map(|marks| DelegatedNetworkProperties { marks, ..Default::default() }),
                dns_servers: dns2,
                connectivity_state: Some(fnp_properties::ConnectivityState::NoConnectivity),
                name: Some(net.name.to_string()),
                network_type: Some(net.network_type),
            }
        );
    }

    #[test]
    fn test_handle_changed_network_validation() {
        let mut networks = RegisteredNetworks::default();
        let delegated =
            Some(DelegatedNetworkProperties { marks: test_marks(), ..Default::default() });
        let net = DELEGATED_NET_1;

        // Update a non-added network
        let event = NetworkPropertiesChange { delegated: delegated.clone(), ..net.added(false) };
        assert_eq!(networks.handle_changed_network(net.id, event), UpdateApplied::None);

        // Add the network
        let event = NetworkPropertiesChange { delegated: delegated.clone(), ..net.added(true) };

        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: true, changed_marks: true, changed_dns: true }),
        );

        // Add already added network
        let event = NetworkPropertiesChange { delegated: delegated.clone(), ..net.added(true) };
        assert_eq!(networks.handle_changed_network(net.id, event), UpdateApplied::None);

        // Fuchsia network with delegated properties
        let fuchsia_net = FUCHSIA_NET_1;
        let event = NetworkPropertiesChange { delegated, ..fuchsia_net.added(true) };
        assert_eq!(networks.handle_changed_network(fuchsia_net.id, event), UpdateApplied::None);

        // Delegated network without delegated properties
        let event = net.added(true);
        assert_eq!(networks.handle_changed_network(net.id, event), UpdateApplied::None);
    }

    // Unit tests the election algorithm directly by manipulating internal state.
    // Verifies prioritization and intermediate fallback election logic.
    #[test]
    fn fallback_election_and_prioritization() {
        let mut networks = RegisteredNetworks::default();

        // Initial State: Empty, no default network.
        assert_eq!(networks.calculate_active_default(), None);

        // Add a delegated network and set as the Starnix default.
        let _ = networks.networks.insert(DELEGATED_ID_1, delegated_properties());
        networks.starnix_default = Some(DELEGATED_ID_1);
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: None })
        );
        assert_eq!(networks.default_network, Some(DELEGATED_ID_1));

        // Replace the delegated network with another delegated network.
        // The new network should take over.
        let _ = networks.networks.insert(DELEGATED_ID_2, delegated_properties());
        networks.starnix_default = Some(DELEGATED_ID_2);
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: Some(DELEGATED_ID_1) })
        );
        assert_eq!(networks.default_network, Some(DELEGATED_ID_2));

        // Add a Fuchsia network. This Fuchsia network should take over because of
        // Fuchsia network priority.
        let _ = networks.networks.insert(FUCHSIA_ID_2, fuchsia_properties());
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: Some(DELEGATED_ID_2) })
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_2));

        // Add a Fuchsia network with a smaller ID. The smaller ID Fuchsia network
        // should take over.
        let _ = networks.networks.insert(FUCHSIA_ID_1, fuchsia_properties());
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: Some(FUCHSIA_ID_2) })
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_1));

        // Remove FUCHSIA_ID_1. The next Fuchsia network should take over.
        let _ = networks.networks.remove(&FUCHSIA_ID_1);
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: Some(FUCHSIA_ID_1) })
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_2));

        // Remove FUCHSIA_ID_2. There are no more Fuchsia networks, so the default
        // should fallback to the delegated network.
        let _ = networks.networks.remove(&FUCHSIA_ID_2);
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: Some(FUCHSIA_ID_2) })
        );
        assert_eq!(networks.default_network, Some(DELEGATED_ID_2));

        // Unset the default delegated network prior to removal.
        assert_eq!(
            networks.apply(NetworkRegistryUpdate::UnsetDefaultNetwork),
            RegistryUpdateResult {
                event: UpdateApplied::None,
                default_changed: Some(DefaultChangedEvent {
                    previous_default: Some(DELEGATED_ID_2)
                })
            }
        );
        assert_eq!(networks.default_network, None);

        // Remove the delegated network.
        assert_eq!(
            networks
                .apply(NetworkRegistryUpdate::ChangeNetwork(DELEGATED_ID_1, NetworkUpdate::Remove)),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkRemoved(DELEGATED_ID_1),
                default_changed: None
            }
        );
    }

    // Tests the integration of `RegisteredNetworks::apply` updates, verifying
    // fallback priority from Fuchsia to Delegated networks and ensuring that
    // active default delegated networks cannot be removed.
    #[test]
    fn test_remove_fuchsia_network_fallback() {
        let mut networks = RegisteredNetworks::default();
        let marks = test_marks();
        let fuchsia_added = NetworkPropertiesChange { added: true, ..Default::default() };

        // Add a Fuchsia network. This should become the default network.
        let result = networks.apply(NetworkRegistryUpdate::ChangeNetwork(
            FUCHSIA_ID_1,
            NetworkUpdate::Properties(fuchsia_added.clone()),
        ));
        assert_matches!(
            result.event,
            UpdateApplied::NetworkChanged { network_id: id, added: true, .. }
            if id == FUCHSIA_ID_1
        );
        assert_eq!(result.default_changed, Some(DefaultChangedEvent { previous_default: None }));

        // Add a second Fuchsia network. This should not change the default network.
        let result = networks.apply(NetworkRegistryUpdate::ChangeNetwork(
            FUCHSIA_ID_2,
            NetworkUpdate::Properties(fuchsia_added),
        ));
        assert_matches!(
            result.event,
            UpdateApplied::NetworkChanged { network_id: id, added: true, .. }
            if id == FUCHSIA_ID_2
        );
        assert_eq!(result.default_changed, None);

        // Add a delegated network. This should not change the default network.
        let result = networks.apply(NetworkRegistryUpdate::ChangeNetwork(
            DELEGATED_ID_1,
            NetworkUpdate::Properties(NetworkPropertiesChange {
                added: true,
                delegated: Some(DelegatedNetworkProperties { marks, ..Default::default() }),
                ..Default::default()
            }),
        ));
        assert_matches!(
            result.event,
            UpdateApplied::NetworkChanged { network_id: id, added: true, .. }
            if id == DELEGATED_ID_1
        );
        assert_eq!(result.default_changed, None);

        // Make the delegated network default (ignored because a Fuchsia
        // network is present).
        let result = networks.apply(NetworkRegistryUpdate::ChangeNetwork(
            DELEGATED_ID_1,
            NetworkUpdate::MakeDefault,
        ));
        assert_eq!(
            result,
            RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_1));

        // Remove the first Fuchsia network (fallback to the second Fuchsia
        // network).
        let result = networks
            .apply(NetworkRegistryUpdate::ChangeNetwork(FUCHSIA_ID_1, NetworkUpdate::Remove));
        assert_eq!(result.event, UpdateApplied::NetworkRemoved(FUCHSIA_ID_1));
        assert_eq!(
            result.default_changed,
            Some(DefaultChangedEvent { previous_default: Some(FUCHSIA_ID_1) })
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_2));

        // Remove the second Fuchsia network (fallback to the
        // delegated network since it is default).
        let result = networks
            .apply(NetworkRegistryUpdate::ChangeNetwork(FUCHSIA_ID_2, NetworkUpdate::Remove));
        assert_eq!(result.event, UpdateApplied::NetworkRemoved(FUCHSIA_ID_2));
        assert_eq!(
            result.default_changed,
            Some(DefaultChangedEvent { previous_default: Some(FUCHSIA_ID_2) })
        );
        assert_eq!(networks.default_network, Some(DELEGATED_ID_1));

        // Remove the delegated network (rejected because it is default).
        let result = networks
            .apply(NetworkRegistryUpdate::ChangeNetwork(DELEGATED_ID_1, NetworkUpdate::Remove));
        assert_eq!(
            result,
            RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
        );
        assert!(networks.networks.contains_key(&DELEGATED_ID_1));
        assert_eq!(networks.default_network, Some(DELEGATED_ID_1));
    }

    #[test]
    fn remove_non_default_fuchsia_preserves_default() {
        let mut networks = RegisteredNetworks::default();

        // Add both networks.
        let _ = networks.networks.insert(FUCHSIA_ID_1, NetworkProperties::default());
        let _ = networks.networks.insert(FUCHSIA_ID_2, NetworkProperties::default());

        // On election, FUCHSIA_ID_1 (the smaller ID) is elected as default.
        assert_eq!(
            networks.handle_default_network_update(),
            Some(DefaultChangedEvent { previous_default: None })
        );
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_1));

        // Remove the non-default network.
        assert_eq!(
            networks
                .apply(NetworkRegistryUpdate::ChangeNetwork(FUCHSIA_ID_2, NetworkUpdate::Remove)),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkRemoved(FUCHSIA_ID_2),
                default_changed: None
            }
        );

        // Verify that FUCHSIA_ID_1 is still the active default.
        assert_eq!(networks.default_network, Some(FUCHSIA_ID_1));
    }

    #[fuchsia::test]
    async fn remove_default_network_cleans_up_tokens() {
        let mut service = NetpolNetworksService::default();

        // Add two Fuchsia networks via ChangeNetwork updates.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                FUCHSIA_ID_1,
                NetworkUpdate::Properties(added_properties(NAME_1)),
            ))
            .await;

        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                FUCHSIA_ID_2,
                NetworkUpdate::Properties(added_properties(NAME_2)),
            ))
            .await;

        // On election, FUCHSIA_ID_1 (the smaller ID) is elected as default.
        assert_eq!(service.default_network(), Some(FUCHSIA_ID_1));

        // Ensure non-default tokens exist for both networks.
        assert!(service.has_token(FUCHSIA_ID_1, false /* is_default */));
        assert!(service.has_token(FUCHSIA_ID_2, false /* is_default */));

        // Manually create default token for FUCHSIA_ID_1 to simulate a client WatchDefault call.
        service.ensure_token_for_test(FUCHSIA_ID_1, true /* is_default */);
        assert!(service.has_token(FUCHSIA_ID_1, true /* is_default */));

        // Remove FUCHSIA_ID_1 (the default network). This should trigger fallback to FUCHSIA_ID_2
        // and clean up FUCHSIA_ID_1's tokens.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(FUCHSIA_ID_1, NetworkUpdate::Remove))
            .await;

        // Verify fallback happened.
        assert_eq!(service.default_network(), Some(FUCHSIA_ID_2));

        // Verify FUCHSIA_ID_1 tokens are gone.
        assert!(!service.has_token(FUCHSIA_ID_1, false /* is_default */));
        assert!(!service.has_token(FUCHSIA_ID_1, true /* is_default */));

        // Verify FUCHSIA_ID_2 tokens still exist.
        assert!(service.has_token(FUCHSIA_ID_2, false /* is_default */));
    }

    #[fuchsia::test]
    async fn test_property_watcher_generation_increment_on_unpolled_update() {
        let mut service = NetpolNetworksService::default();

        // Set up initial network state and register a PropertyWatcher connection.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    true,
                    Some(test_marks()),
                    None,
                    None,
                )),
            ))
            .await;

        let token = service
            .tokens
            .ensure_token(NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
            .get()
            .duplicate()
            .unwrap();

        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();

        service.add_stream(networks_stream);

        // Watch for socket marks changes on `DELEGATED_ID_1` and expect the initial state.
        let request = fnp_properties::NetworksWatchPropertiesRequest {
            network: Some(token),
            properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
            watcher: Some(server_end),
            ..Default::default()
        };

        let watch_req = networks_proxy.watch_properties(request);
        let req_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(watch_req.await, Ok(Ok(())));

        // First watch call should return the initial state.
        let watch_fut1 = watcher.watch();
        let pw_event1 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event1).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let initial_updates = watch_fut1.await.unwrap().unwrap();
        assert_eq!(
            initial_updates,
            fnp_properties::PropertyUpdate {
                socket_marks: Some(test_marks()),
                dns_configuration: None,
                ..Default::default()
            }
        );

        // Update the network properties while the client is idle, ensuring the server increments
        // its generation without bumping the unpolled client's generation.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    false,
                    Some(test_marks_updated()),
                    None,
                    None,
                )),
            ))
            .await;

        // Verify the pending change is immediately available to the client.
        let watch_fut2 = watcher.watch();
        let pw_event2 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event2).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let updates = watch_fut2.await.unwrap().unwrap();
        assert_eq!(
            updates,
            fnp_properties::PropertyUpdate {
                socket_marks: Some(test_marks_updated()),
                dns_configuration: None,
                ..Default::default()
            }
        );
    }

    #[fuchsia::test]
    async fn test_watch_should_return_error_on_concurrent_call() {
        let mut service = NetpolNetworksService::default();

        let token = service
            .tokens
            .ensure_token(NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
            .get()
            .duplicate()
            .unwrap();

        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();

        service.add_stream(networks_stream);

        let watch_req =
            networks_proxy.watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                network: Some(token),
                properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
                watcher: Some(server_end),
                ..Default::default()
            });
        let req_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(watch_req.await, Ok(Ok(())));

        // Call `watch()` twice concurrently on the same connection.
        let watch_fut1 = watcher.watch();
        let pw_event1 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event1).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        let watch_fut2 = watcher.watch();
        let pw_event2 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event2).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        for res in [watch_fut1.await, watch_fut2.await] {
            assert_matches!(
                res,
                Err(fidl::Error::ClientChannelClosed { epitaph, .. })
                    if epitaph == zx::Status::ALREADY_EXISTS
            );
        }
        // The registration is removed when the closed stream yields its terminal item.
        let disconnect_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(disconnect_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert!(service.property_watchers.is_empty());
    }

    #[fuchsia::test]
    async fn test_watch_default_should_return_error_on_concurrent_call() {
        let mut service = NetpolNetworksService::default();

        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();

        service.add_stream(networks_stream);

        // Call `watch_default` twice concurrently on the same connection.
        let watch_fut1 = networks_proxy.watch_default();
        let req_event1 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event1).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        let watch_fut2 = networks_proxy.watch_default();
        let req_event2 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event2).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        for res in [watch_fut1.await, watch_fut2.await] {
            assert_matches!(
                res,
                Err(fidl::Error::ClientChannelClosed { epitaph, .. })
                    if epitaph == zx::Status::ALREADY_EXISTS
            );
        }
        // The client is removed when the closed stream yields its terminal item.
        let disconnect_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(disconnect_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert!(service.networks_clients.is_empty());
    }

    #[fuchsia::test]
    async fn test_network_removal_reports_network_gone() {
        let mut service = NetpolNetworksService::default();

        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    true,
                    Some(test_marks()),
                    None,
                    None,
                )),
            ))
            .await;

        let token1 = service
            .tokens
            .ensure_token(NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
            .get()
            .duplicate()
            .unwrap();
        let token2 = service
            .tokens
            .ensure_token(NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
            .get()
            .duplicate()
            .unwrap();

        let (active_watcher, active_server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let (idle_watcher, idle_server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();

        service.add_stream(networks_stream);

        for (token, server_end) in [(token1, active_server_end), (token2, idle_server_end)] {
            let watch_req =
                networks_proxy.watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                    network: Some(token),
                    properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
                    watcher: Some(server_end),
                    ..Default::default()
                });
            let req_event = service.select_next_some().await;
            assert_eq!(
                service.handle_event(req_event).await.expect("Failed to handle event"),
                DelegatedNetworkUpdateResult::default()
            );
            assert_matches!(watch_req.await, Ok(Ok(())));
        }

        // Active watcher's first watch call returns the initial snapshot.
        let active_watch_fut1 = active_watcher.watch();
        let active_pw_event1 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(active_pw_event1).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let initial_updates = active_watch_fut1.await.unwrap().unwrap();
        assert_eq!(
            initial_updates,
            fnp_properties::PropertyUpdate {
                socket_marks: Some(test_marks()),
                dns_configuration: None,
                ..Default::default()
            }
        );

        // Active watcher starts an in-flight watch call before network removal.
        let mut active_watch_fut2 = active_watcher.watch();
        let active_pw_event2 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(active_pw_event2).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!((&mut active_watch_fut2).now_or_never(), None);

        // Remove the network.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(DELEGATED_ID_1, NetworkUpdate::Remove))
            .await;

        // Active watcher immediately observes NetworkGone on its in-flight call, and its
        // stream yields the terminal item because `update` closed the channel.
        assert_matches!(
            active_watch_fut2.await,
            Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
        );
        let active_closed_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(active_closed_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        // Idle watcher calls watch() after network removal and also observes NetworkGone.
        let idle_watch_fut = idle_watcher.watch();
        let idle_pw_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(idle_pw_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(
            idle_watch_fut.await,
            Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
        );
    }

    #[fuchsia::test]
    async fn test_idle_default_watcher_reports_network_gone_on_default_change() {
        let mut service = NetpolNetworksService::default();

        // Add network and set it as the default network.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    true,
                    Some(test_marks()),
                    None,
                    None,
                )),
            ))
            .await;
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::MakeDefault,
            ))
            .await;

        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
        service.add_stream(networks_stream);

        // Client calls watch_default() to get default token.
        let default_fut = networks_proxy.watch_default();
        let req_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let default_token = match default_fut.await.unwrap() {
            fnp_properties::NetworksWatchDefaultResponse::Network(token) => token,
            res => panic!("Expected Network token, got {res:?}"),
        };

        // Client registers PropertyWatcher for the default token.
        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let watch_req =
            networks_proxy.watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                network: Some(default_token),
                properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
                watcher: Some(server_end),
                ..Default::default()
            });
        let req_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(watch_req.await, Ok(Ok(())));

        // Client fetches initial property snapshot.
        let initial_fut = watcher.watch();
        let pw_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(initial_fut.await, Ok(Ok(_)));

        // Client has no pending watch calls. Unset the default network.
        service.update(NetworkRegistryUpdate::unset_default()).await;

        // Idle watcher calls watch() after default network change and receives NetworkGone.
        let idle_watch_fut = watcher.watch();
        let idle_pw_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(idle_pw_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(
            idle_watch_fut.await,
            Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
        );
    }

    #[test_case(
        None,
        Some(fnp_properties::NetworkInfo::Starnix(
            fnp_properties::StarnixNetworkInfo {
                mark: Some(123),
                handle: Some(1),
                ..Default::default()
            }
        )),
        Err(NetworkValidationError::MissingNetworkId);
        "missing network_id"
    )]
    #[test_case(
        Some(0),
        Some(fnp_properties::NetworkInfo::Starnix(
            fnp_properties::StarnixNetworkInfo {
                mark: Some(123),
                handle: Some(1),
                ..Default::default()
            }
        )),
        Err(NetworkValidationError::MissingNetworkId);
        "zero network_id"
    )]
    #[test_case(
        Some(1),
        None,
        Err(NetworkValidationError::MissingNetworkInfo);
        "missing network_info"
    )]
    #[test_case(
        Some(1),
        Some(fnp_properties::NetworkInfo::Fuchsia(Default::default())),
        Err(NetworkValidationError::MissingNetworkInfo);
        "fuchsia network_info"
    )]
    #[test_case(
        Some(1),
        Some(fnp_properties::NetworkInfo::Starnix(
            fnp_properties::StarnixNetworkInfo {
                mark: Some(123),
                handle: None,
                ..Default::default()
            }
        )),
        Err(NetworkValidationError::MissingNetworkInfo);
        "missing starnix handle"
    )]
    #[test_case(
        Some(1),
        Some(fnp_properties::NetworkInfo::Starnix(
            fnp_properties::StarnixNetworkInfo {
                mark: Some(123),
                handle: Some(1),
                ..Default::default()
            }
        )),
        Ok(NetworkId::delegated(InterfaceId::new(1).unwrap()));
        "valid starnix network"
    )]
    fn test_validated_network_validation(
        network_id: Option<u32>,
        info: Option<fnp_properties::NetworkInfo>,
        expected: Result<NetworkId, NetworkValidationError>,
    ) {
        let net = fnp_properties::Network { network_id, info, ..Default::default() };
        let result = ValidatedNetwork::try_from(net).map(|v| v.network_id);
        assert_eq!(result, expected);
    }

    fn starnix_network_payload(id: u32, mark: u32) -> fnp_properties::Network {
        fnp_properties::Network {
            network_id: Some(id),
            info: Some(NetworkInfo::Starnix(StarnixNetworkInfo {
                mark: Some(mark),
                handle: Some(id.into()),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    async fn process_fidl_request<E>(
        service: &mut NetpolNetworksService,
        request_future: impl Future<Output = Result<Result<(), E>, fidl::Error>>,
    ) -> Result<(), E> {
        let event = service.select_next_some().await;
        let update_result = service.handle_event(event).await.expect("failed to handle event");
        let client_result = request_future.await.expect("FIDL call failed");
        assert_eq!(update_result.dns_servers.is_some(), client_result.is_ok());
        client_result
    }

    #[fuchsia::test]
    async fn test_delegated_networks_fidl_errors() {
        let mut service = NetpolNetworksService::default();
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream);

        const NETWORK_ID_1: u32 = 1;
        const NETWORK_ID_2: u32 = 2;
        const SOCKET_MARK: u32 = 100;

        // Updating a network before it has been added returns NotFound.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.update(&starnix_network_payload(NETWORK_ID_1, SOCKET_MARK,)),
            )
            .await,
            Err(NetworkRegistryUpdateError::NotFound)
        );

        // Adding a valid network succeeds.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.add(&starnix_network_payload(NETWORK_ID_1, SOCKET_MARK)),
            )
            .await,
            Ok(())
        );

        // Adding a duplicate network ID returns DuplicateNetworkId.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.add(&starnix_network_payload(NETWORK_ID_1, SOCKET_MARK)),
            )
            .await,
            Err(NetworkRegistryAddError::DuplicateNetworkId)
        );

        // Removing a non-existent network returns NotFound.
        assert_eq!(
            process_fidl_request(&mut service, proxy.remove(NETWORK_ID_2)).await,
            Err(NetworkRegistryRemoveError::NotFound)
        );

        // Setting a non-existent network as default returns NotFound.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Value(NETWORK_ID_2,)),
            )
            .await,
            Err(NetworkRegistrySetDefaultError::NotFound)
        );

        // Setting an existing network as default succeeds.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Value(NETWORK_ID_1,)),
            )
            .await,
            Ok(())
        );

        // Attempting to remove the active default network returns
        // CannotRemoveDefaultNetwork.
        assert_eq!(
            process_fidl_request(&mut service, proxy.remove(NETWORK_ID_1)).await,
            Err(NetworkRegistryRemoveError::CannotRemoveDefaultNetwork)
        );

        // Unsetting the default network succeeds.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Unset(fposix_socket::Empty)),
            )
            .await,
            Ok(())
        );

        // Once unset from default, removing the network succeeds.
        assert_eq!(process_fidl_request(&mut service, proxy.remove(NETWORK_ID_1)).await, Ok(()));
    }

    #[fuchsia::test]
    async fn test_inspect_metrics() {
        let inspector = fuchsia_inspect::Inspector::default();
        let telemetry_node = inspector.root().create_child("telemetry");
        let mut service = NetpolNetworksService::default()
            .with_inspect(&telemetry_node, "operations", &telemetry_node, "network_registry")
            .expect("failed to initialize inspect");
        inspector.root().record(telemetry_node);

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream);

        let make_network = |id: u32, mark: u32| fnp_properties::Network {
            name: Some("wlan0".to_string()),
            network_type: Some(fnp_properties::NetworkType::Wifi),
            connectivity: Some(fnp_properties::ConnectivityState::FullConnectivity),
            ..starnix_network_payload(id, mark)
        };

        // Initial check - all values should be default.
        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            adds: contains { successes: 0u64, errors: 0u64 },
                            removes: contains { successes: 0u64, errors: 0u64 },
                            set_defaults: contains {
                                successes: 0u64,
                                errors: 0u64,
                            },
                            updates: contains {
                                successes: 0u64,
                                errors: 0u64,
                            },
                        }
                    }
                }
            }
        );

        // Add valid network 1.
        assert_eq!(
            process_fidl_request(&mut service, proxy.add(&make_network(1, 100))).await,
            Ok(())
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            adds: contains { successes: 1u64, errors: 0u64 },
                        }
                    }
                }
            }
        );

        // Add duplicate network 1 -> error.
        assert_eq!(
            process_fidl_request(&mut service, proxy.add(&make_network(1, 100))).await,
            Err(NetworkRegistryAddError::DuplicateNetworkId)
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            adds: contains { successes: 1u64, errors: 1u64 },
                        }
                    }
                }
            }
        );

        // Update network 1 properties.
        assert_eq!(
            process_fidl_request(&mut service, proxy.update(&make_network(1, 150))).await,
            Ok(())
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            updates: contains {
                                successes: 1u64,
                                errors: 0u64,
                            },
                        }
                    }
                }
            }
        );

        // Make network 1 default.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Value(1)),
            )
            .await,
            Ok(())
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            set_defaults: contains {
                                successes: 1u64,
                                errors: 0u64,
                            },
                        }
                    }
                }
            }
        );

        // Make non-existent network 2 default -> error.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Value(2)),
            )
            .await,
            Err(NetworkRegistrySetDefaultError::NotFound)
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            set_defaults: contains {
                                successes: 1u64,
                                errors: 1u64,
                            },
                        }
                    }
                }
            }
        );

        // Try removing default network 1 -> error.
        assert_eq!(
            process_fidl_request(&mut service, proxy.remove(1)).await,
            Err(NetworkRegistryRemoveError::CannotRemoveDefaultNetwork)
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            removes: contains { successes: 0u64, errors: 1u64 },
                        }
                    }
                }
            }
        );

        // Unset default.
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Unset(fposix_socket::Empty)),
            )
            .await,
            Ok(())
        );

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            set_defaults: contains {
                                successes: 2u64,
                                errors: 1u64,
                            },
                        }
                    }
                }
            }
        );

        // Remove network 1 -> success.
        assert_eq!(process_fidl_request(&mut service, proxy.remove(1)).await, Ok(()));

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    operations: contains {
                        delegated: contains {
                            removes: contains { successes: 1u64, errors: 1u64 },
                        }
                    }
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_network_registry_inspect() {
        const FUCHSIA_ID_2: NetworkId = NetworkId::Fuchsia(ID_2);
        const DELEGATED_ID_100: NetworkId =
            NetworkId::Delegated(InterfaceId(NonZeroU64::new(100).unwrap()));

        let inspector = fuchsia_inspect::Inspector::default();
        let telemetry_node = inspector.root().create_child("telemetry");
        let mut service = NetpolNetworksService::default()
            .with_inspect(&telemetry_node, "operations", &telemetry_node, "network_registry")
            .expect("failed to initialize inspect");
        inspector.root().record(telemetry_node);

        // Initial check: no networks, reported both as a zero count and as an
        // empty network_registry node.
        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_count: 0u64,
                    network_registry: {}
                }
            }
        );

        // Add a Fuchsia network (ID_2)
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                FUCHSIA_ID_2,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    name: Some("wlan0".to_string()),
                    network_type: Some(fnp_properties::NetworkType::Wifi),
                    connectivity_state: Some(fnp_properties::ConnectivityState::FullConnectivity),
                    ..Default::default()
                }),
            ))
            .await;

        // Add a Delegated network (ID 100)
        let mut marks = fnet::Marks::default();
        marks.mark_1 = Some(100);
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_100,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    delegated: Some(DelegatedNetworkProperties {
                        marks: marks.clone(),
                        ..Default::default()
                    }),
                    name: Some("eth0".to_string()),
                    network_type: Some(fnp_properties::NetworkType::Ethernet),
                    connectivity_state: Some(fnp_properties::ConnectivityState::LocalConnectivity),
                    ..Default::default()
                }),
            ))
            .await;

        // Make Delegated network Starnix default
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_100,
                NetworkUpdate::MakeDefault,
            ))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_count: 2u64,
                    network_registry: {
                        default_network: "fuchsia:2",
                        starnix_default: "delegated:100",
                        fuchsia_2: {
                            name: "wlan0",
                            network_type: "Wifi",
                            connectivity_state: "FullConnectivity",
                        },
                        delegated_100: {
                            name: "eth0",
                            network_type: "Ethernet",
                            connectivity_state: "LocalConnectivity",
                        },
                    }
                }
            }
        );

        // Update Delegated network properties (change connectivity_state and name)
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_100,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: false,
                    delegated: Some(DelegatedNetworkProperties { marks, ..Default::default() }),
                    name: Some("eth0_updated".to_string()),
                    network_type: Some(fnp_properties::NetworkType::Ethernet),
                    connectivity_state: Some(fnp_properties::ConnectivityState::FullConnectivity),
                    ..Default::default()
                }),
            ))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_count: 2u64,
                    network_registry: {
                        default_network: "fuchsia:2",
                        starnix_default: "delegated:100",
                        fuchsia_2: {
                            name: "wlan0",
                            network_type: "Wifi",
                            connectivity_state: "FullConnectivity",
                        },
                        delegated_100: {
                            name: "eth0_updated",
                            network_type: "Ethernet",
                            connectivity_state: "FullConnectivity",
                        },
                    }
                }
            }
        );

        // Remove the Fuchsia network (ID_2).
        // Default network should now fall back to the delegated network.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(FUCHSIA_ID_2, NetworkUpdate::Remove))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_count: 1u64,
                    network_registry: {
                        default_network: "delegated:100",
                        starnix_default: "delegated:100",
                        delegated_100: {
                            name: "eth0_updated",
                            network_type: "Ethernet",
                            connectivity_state: "FullConnectivity",
                        },
                    }
                }
            }
        );

        // Unset Starnix default and remove Delegated network.
        // network_registry node should now be completely empty.
        service.update(NetworkRegistryUpdate::UnsetDefaultNetwork).await;
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(DELEGATED_ID_100, NetworkUpdate::Remove))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_count: 0u64,
                    network_registry: {}
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_reachability_service_integration() {
        let mut service = NetpolNetworksService::default();

        let mut expected_disconnected = freachability::Snapshot::default();
        expected_disconnected.gateway_reachable = Some(false);
        expected_disconnected.internet_available = Some(false);
        expected_disconnected.dns_active = Some(false);
        expected_disconnected.http_active = Some(false);

        let mut expected_validated = freachability::Snapshot::default();
        expected_validated.gateway_reachable = Some(true);
        expected_validated.internet_available = Some(true);
        expected_validated.dns_active = Some(true);
        expected_validated.http_active = Some(true);

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<freachability::MonitorMarker>();
        service.add_stream(stream);
        assert_eq!(service.reachability_watcher_count(), 1);

        let watch_fut = proxy.watch();
        let event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(event).await.expect("handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let snapshot = watch_fut.await.expect("watch error");
        assert_eq!(snapshot, expected_disconnected);

        let mut second_watch = proxy.watch();
        let event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(event).await.expect("handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(futures::poll!(&mut second_watch), std::task::Poll::Pending);

        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    true,
                    Some(test_marks()),
                    None,
                    Some(fnp_properties::ConnectivityState::FullConnectivity),
                )),
            ))
            .await;
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::MakeDefault,
            ))
            .await;

        let snapshot = second_watch.await.expect("watch should succeed");
        assert_eq!(snapshot, expected_validated);
    }

    #[fuchsia::test]
    async fn test_networks_client_disconnect_cleans_up_state() {
        let mut service = NetpolNetworksService::default();
        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
        service.add_stream(stream);

        // Client calls WatchDefault when there is no default network (so it hangs).
        let mut watch_default_fut = proxy.watch_default();
        let event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!((&mut watch_default_fut).now_or_never(), None);
        assert_matches!(
            &service.networks_clients.values().collect::<Vec<_>>()[..],
            [NetworksClient { responder: Some(_), .. }]
        );

        // Client closes the connection.
        drop(watch_default_fut);
        drop(proxy);

        let event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        // Client state and connection generations must be cleaned up.
        assert!(service.networks_clients.is_empty());
        assert!(service.generations_by_connection.default_network.is_empty());
    }

    #[fuchsia::test]
    async fn test_property_watcher_client_disconnect_cleans_up_state() {
        let mut service = NetpolNetworksService::default();

        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(DELEGATED_NET_1.change_with(
                    true,
                    Some(test_marks()),
                    None,
                    None,
                )),
            ))
            .await;

        let token = service
            .tokens
            .ensure_token(NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
            .get()
            .duplicate()
            .unwrap();

        let (watcher, server_end) =
            fidl::endpoints::create_proxy::<fnp_properties::PropertyWatcherMarker>();
        let (networks_proxy, networks_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnp_properties::NetworksMarker>();
        service.add_stream(networks_stream);

        let watch_req =
            networks_proxy.watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                network: Some(token),
                properties: Some(fnp_properties::PropertyInterest::SOCKET_MARKS),
                watcher: Some(server_end),
                ..Default::default()
            });

        let req_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(req_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!(watch_req.await, Ok(Ok(())));

        // Consume initial snapshot.
        let watch_fut1 = watcher.watch();
        let pw_event1 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event1).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        let _ = watch_fut1.await.unwrap().unwrap();

        // Second watch hangs waiting for updates.
        let mut watch_fut2 = watcher.watch();
        let pw_event2 = service.select_next_some().await;
        assert_eq!(
            service.handle_event(pw_event2).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert_matches!((&mut watch_fut2).now_or_never(), None);
        assert_eq!(service.property_watchers.len(), 1);

        // Client closes the PropertyWatcher connection.
        drop(watch_fut2);
        drop(watcher);

        let disconnect_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(disconnect_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );

        // Watcher registration and connection generation must be cleanly purged.
        assert!(service.property_watchers.is_empty());
        assert!(service.generations_by_connection.properties.is_empty());
    }
    #[fuchsia::test]
    async fn test_delegated_networks_single_connection() {
        let mut service = NetpolNetworksService::default();
        let (proxy1, stream1) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream1);
        assert!(service.delegated_networks_client.is_some());

        // First connection succeeds.
        assert_eq!(
            process_fidl_request(&mut service, proxy1.remove(1)).await,
            Err(NetworkRegistryRemoveError::NotFound)
        );

        // Concurrent second connection is rejected with ALREADY_EXISTS.
        let (proxy2, stream2) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream2);
        assert_matches!(
            proxy2.remove(1).await,
            Err(fidl::Error::ClientChannelClosed { epitaph, .. })
                if epitaph == zx::Status::ALREADY_EXISTS
        );

        // First connection continues to work after the second connection is rejected.
        assert_eq!(
            process_fidl_request(&mut service, proxy1.remove(1)).await,
            Err(NetworkRegistryRemoveError::NotFound)
        );

        // Once the first connection closes and its terminal item is processed,
        // a new connection is allowed.
        drop(proxy1);
        let disconnect_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(disconnect_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        assert!(service.delegated_networks_client.is_none());

        let (proxy3, stream3) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream3);
        assert_eq!(
            process_fidl_request(&mut service, proxy3.remove(1)).await,
            Err(NetworkRegistryRemoveError::NotFound)
        );
    }

    #[fuchsia::test]
    async fn test_delegated_networks_client_disconnect_cleans_up_state() {
        let mut service = NetpolNetworksService::default();
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream);

        // The raw id used by the `fnp_properties::Network` as a `network_id`.
        const STARNIX_ID_1: u32 = 1;
        const STARNIX_ID_2: u32 = 2;
        const SOCKET_MARK_1: u32 = 100;
        const SOCKET_MARK_2: u32 = 200;

        // Register a native Fuchsia network alongside two delegated networks.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                FUCHSIA_ID_1,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    ..Default::default()
                }),
            ))
            .await;

        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.add(&starnix_network_payload(STARNIX_ID_1, SOCKET_MARK_1))
            )
            .await,
            Ok(())
        );
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.add(&starnix_network_payload(STARNIX_ID_2, SOCKET_MARK_2))
            )
            .await,
            Ok(())
        );
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy.set_default(&fposix_socket::OptionalUint32::Value(STARNIX_ID_1)),
            )
            .await,
            Ok(())
        );
        assert_eq!(service.network_registry.starnix_default, Some(DELEGATED_ID_1));
        assert_eq!(service.network_registry.networks.len(), 3);

        // Disconnect the client and verify all delegated state is cleared while Fuchsia state
        // remains intact.
        drop(proxy);
        let disconnect_event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(disconnect_event).await.expect("Failed to handle event"),
            DelegatedNetworkUpdateResult { dns_servers: Some(vec![]) }
        );
        assert_eq!(service.network_registry.starnix_default, None);
        assert_eq!(
            service.network_registry.networks.keys().copied().collect::<Vec<_>>(),
            vec![FUCHSIA_ID_1]
        );

        // A reconnecting client can re-add NETWORK_ID_1 without hitting DuplicateNetworkId.
        let (proxy2, stream2) = fidl::endpoints::create_proxy_and_stream::<NetworkRegistryMarker>();
        service.add_stream(stream2);
        assert_eq!(
            process_fidl_request(
                &mut service,
                proxy2.add(&starnix_network_payload(STARNIX_ID_1, SOCKET_MARK_2)),
            )
            .await,
            Ok(())
        );
    }

    #[fuchsia::test]
    async fn test_networks_and_dns_inspect() {
        const FUCHSIA_ID_2: NetworkId = NetworkId::Fuchsia(ID_2);
        const DELEGATED_ID_100: NetworkId =
            NetworkId::Delegated(InterfaceId(NonZeroU64::new(100).unwrap()));

        let inspector = fuchsia_inspect::Inspector::default();
        let telemetry_node = inspector.root().create_child("telemetry");
        let mut service = NetpolNetworksService::default()
            .with_inspect(&telemetry_node, "operations", &telemetry_node, "network_registry")
            .expect("failed to initialize inspect");
        inspector.root().record(telemetry_node);

        // Initial check: empty network_registry node
        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_registry: {}
                }
            }
        );

        // Update DNS with static and dynamic (DHCPv4, NDP, DHCPv6) servers.
        let static_server_1 = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("8.8.8.8:53")),
            source: Some(fnet_name::DnsServerSource::StaticSource(
                fnet_name::StaticDnsServerSource::default(),
            )),
            ..Default::default()
        };
        let static_server_2 = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("8.8.4.4:53")),
            source: Some(fnet_name::DnsServerSource::StaticSource(
                fnet_name::StaticDnsServerSource::default(),
            )),
            ..Default::default()
        };
        let dhcp_server = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("192.168.1.1:53")),
            source: Some(fnet_name::DnsServerSource::Dhcp(fnet_name::DhcpDnsServerSource {
                source_interface: Some(ID_2.get()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let ndp_server = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("[fe80::1%1]:53")),
            source: Some(fnet_name::DnsServerSource::Ndp(fnet_name::NdpDnsServerSource {
                source_interface: Some(ID_2.get()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let dhcpv6_server = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("[2001:4860:4860::8888]:53")),
            source: Some(fnet_name::DnsServerSource::Dhcpv6(fnet_name::Dhcpv6DnsServerSource {
                source_interface: Some(ID_2.get()),
                ..Default::default()
            })),
            ..Default::default()
        };

        service
            .update(NetworkRegistryUpdate::UpdateDns(vec![
                dhcp_server.clone(),
                ndp_server.clone(),
                dhcpv6_server.clone(),
                static_server_1.clone(),
                static_server_2.clone(),
            ]))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_registry: contains {
                        system_dns_servers: {
                            "0": {
                                server: "192.168.1.1:53",
                                type: "Dhcpv4",
                                interface_id: 2u64,
                            },
                            "1": {
                                server: "[fe80::1%1]:53",
                                type: "Ndp",
                                interface_id: 2u64,
                            },
                            "2": {
                                server: "[2001:4860:4860::8888]:53",
                                type: "Dhcpv6",
                                interface_id: 2u64,
                            },
                            "3": {
                                server: "8.8.8.8:53",
                                type: "Static",
                            },
                            "4": {
                                server: "8.8.4.4:53",
                                type: "Static",
                            },
                        },
                    }
                }
            }
        );

        // Add a Fuchsia network (ID_2)
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                FUCHSIA_ID_2,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    name: Some("wlan0".to_string()),
                    network_type: Some(fnp_properties::NetworkType::Wifi),
                    connectivity_state: Some(fnp_properties::ConnectivityState::FullConnectivity),
                    ..Default::default()
                }),
            ))
            .await;

        // Add a Delegated network (ID 100) with SocketProxy DNS.
        let socketproxy_dns = fnet_name::DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("192.168.1.2:53")),
            source: Some(fnet_name::DnsServerSource::SocketProxy(
                fnet_name::SocketProxyDnsServerSource::default(),
            )),
            ..Default::default()
        };
        let mut marks = fnet::Marks::default();
        marks.mark_1 = Some(100);
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_100,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    delegated: Some(DelegatedNetworkProperties { marks, ..Default::default() }),
                    dns_servers: Some(vec![socketproxy_dns]),
                    name: Some("wlan0".to_string()),
                    network_type: Some(fnp_properties::NetworkType::Wifi),
                    connectivity_state: Some(fnp_properties::ConnectivityState::FullConnectivity),
                }),
            ))
            .await;

        let hierarchy = fuchsia_inspect::reader::read(&inspector).await.unwrap();
        assert_data_tree!(
            hierarchy,
            root: contains {
                telemetry: contains {
                    network_registry: {
                        default_network: "fuchsia:2",
                        system_dns_servers: {
                            "0": {
                                server: "192.168.1.1:53",
                                type: "Dhcpv4",
                                interface_id: 2u64,
                            },
                            "1": {
                                server: "[fe80::1%1]:53",
                                type: "Ndp",
                                interface_id: 2u64,
                            },
                            "2": {
                                server: "[2001:4860:4860::8888]:53",
                                type: "Dhcpv6",
                                interface_id: 2u64,
                            },
                            "3": {
                                server: "8.8.8.8:53",
                                type: "Static",
                            },
                            "4": {
                                server: "8.8.4.4:53",
                                type: "Static",
                            },
                        },
                        fuchsia_2: {
                            name: "wlan0",
                            network_type: "Wifi",
                            connectivity_state: "FullConnectivity",
                            dns_servers: {
                                "0": {
                                    server: "192.168.1.1:53",
                                    type: "Dhcpv4",
                                    interface_id: 2u64,
                                },
                                "1": {
                                    server: "[fe80::1%1]:53",
                                    type: "Ndp",
                                    interface_id: 2u64,
                                },
                                "2": {
                                    server: "[2001:4860:4860::8888]:53",
                                    type: "Dhcpv6",
                                    interface_id: 2u64,
                                },
                            },
                        },
                        delegated_100: {
                            name: "wlan0",
                            network_type: "Wifi",
                            connectivity_state: "FullConnectivity",
                            dns_servers: {
                                "0": {
                                    server: "192.168.1.2:53",
                                    type: "SocketProxy",
                                },
                            },
                        },
                    }
                }
            }
        );
    }

    async fn drive_request<T>(
        service: &mut NetpolNetworksService,
        request_future: impl Future<Output = Result<T, fidl::Error>>,
    ) -> T {
        let event = service.select_next_some().await;
        assert_eq!(
            service.handle_event(event).await.expect("failed to handle event"),
            DelegatedNetworkUpdateResult::default()
        );
        request_future.await.expect("FIDL call failed")
    }

    #[fuchsia::test]
    async fn test_lookup_network() {
        let mut service = NetpolNetworksService::default();
        let (lookup_proxy, lookup_stream) = fidl::endpoints::create_proxy_and_stream::<
            fnp_properties::PrivilegedNetworkLookupMarker,
        >();
        service.add_stream(lookup_stream);

        const STARNIX_HANDLE: u64 = 0x1111;
        const STARNIX_HANDLE_UPDATED: u64 = 0x2222;

        let set_handle = |added: bool, starnix_handle: u64| {
            NetworkRegistryUpdate::ChangeNetwork(
                DELEGATED_ID_1,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added,
                    delegated: Some(DelegatedNetworkProperties {
                        marks: test_marks(),
                        starnix_handle,
                    }),
                    ..Default::default()
                }),
            )
        };
        let lookup = |handle: u64| {
            lookup_proxy.lookup(&fnp_properties::LookupRequest::StarnixHandle(handle))
        };

        // Lookup before any networks are added returns NotFound.
        assert_eq!(
            drive_request(&mut service, lookup(STARNIX_HANDLE)).await,
            Err(fnp_properties::LookupError::NotFound)
        );

        // Add DELEGATED_ID_1 as a non-default network and look up its token by STARNIX_HANDLE.
        service.update(set_handle(true, STARNIX_HANDLE)).await;
        let token_1 = drive_request(&mut service, lookup(STARNIX_HANDLE)).await.unwrap();
        assert_eq!(
            service.tokens.get_contents(&token_1),
            Ok(&NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
        );

        // Updating DELEGATED_ID_1's handle causes future lookups by STARNIX_HANDLE to return
        // NotFound and lookups by STARNIX_HANDLE_UPDATED to return a token with the same KOID,
        // while the previously obtained `token_1` remains active.
        service.update(set_handle(false, STARNIX_HANDLE_UPDATED)).await;
        assert_eq!(
            drive_request(&mut service, lookup(STARNIX_HANDLE)).await,
            Err(fnp_properties::LookupError::NotFound)
        );
        assert_eq!(
            service.tokens.get_contents(&token_1),
            Ok(&NetworkTokenContents { network_id: DELEGATED_ID_1, is_default: false })
        );
        let token_1_updated =
            drive_request(&mut service, lookup(STARNIX_HANDLE_UPDATED)).await.unwrap();
        assert_eq!(token_1.koid().unwrap(), token_1_updated.koid().unwrap());

        // Removing DELEGATED_ID_1 invalidates `token_1` and causes Lookup to return NotFound.
        service
            .update(NetworkRegistryUpdate::ChangeNetwork(DELEGATED_ID_1, NetworkUpdate::Remove))
            .await;
        assert_eq!(service.tokens.get_contents(&token_1), Err(zx::Status::NOT_FOUND));
        assert_eq!(
            drive_request(&mut service, lookup(STARNIX_HANDLE_UPDATED)).await,
            Err(fnp_properties::LookupError::NotFound)
        );
    }
}
