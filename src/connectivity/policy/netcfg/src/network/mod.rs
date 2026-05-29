// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InterfaceId;
use crate::dns::DNS_PORT;
use crate::telemetry::{NetworkEventMetadata, TelemetryEvent, TelemetrySender};
use anyhow::Context as _;
use async_utils::stream::{Tagged, WithTag as _};
use dns_server_watcher::DnsServers;
use fidl::endpoints::{ControlHandle as _, Responder as _};
use log::{error, info, warn};
use policy_properties::NetworkTokenExt as _;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

mod token_registry;

use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_name as fnet_name;
use fidl_fuchsia_net_policy_properties as fnp_properties;
use fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy;
use fidl_fuchsia_posix_socket as fposix_socket;

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

    pub fn is_delegated(&self) -> bool {
        matches!(self, NetworkId::Delegated(_))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NetworkTokenContents {
    network_id: NetworkId,
    is_default: bool,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(usize);

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
pub struct UpdateGenerations(HashMap<ConnectionId, UpdateGeneration>);

impl UpdateGenerations {
    fn default_network(&self, id: &ConnectionId) -> Option<usize> {
        self.0.get(id).map(|g| g.default_network)
    }

    fn set_default_network(&mut self, id: ConnectionId, generation: UpdateGeneration) {
        self.0.entry(id).or_default().default_network = generation.default_network;
    }

    fn properties(&self, id: &ConnectionId) -> Option<usize> {
        self.0.get(id).map(|g| g.properties)
    }

    fn set_properties(&mut self, id: ConnectionId, generation: UpdateGeneration) {
        self.0.entry(id).or_default().properties = generation.properties;
    }

    fn remove(&mut self, id: &ConnectionId) -> Option<UpdateGeneration> {
        self.0.remove(id)
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

#[derive(Debug)]
pub(crate) struct NetworkPropertyResponder {
    token: fnp_properties::NetworkToken,
    watched_properties: Vec<fnp_properties::Property>,
    responder: fnp_properties::NetworksWatchPropertiesResponder,
}

impl NetworkPropertyResponder {
    fn respond(
        self,
        response: Result<&[fnp_properties::PropertyUpdate], fnp_properties::WatchError>,
    ) -> Result<(), fidl::Error> {
        self.responder.send(response)
    }
}

#[derive(Default, Clone)]
struct NetworkProperties {
    socket_marks: Option<fnet::Marks>,
    dns_servers: Vec<fnet_name::DnsServer_>,
    // TODO(https://fxbug.dev/486892417): Use this field for snapshot metrics.
    #[allow(dead_code)]
    connectivity_state: Option<fnp_socketproxy::ConnectivityState>,
    name: Option<String>,
    network_type: Option<fnp_socketproxy::NetworkType>,
}

impl NetworkProperties {
    fn get_marks(&self) -> Option<&fnet::Marks> {
        self.socket_marks.as_ref()
    }
}

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

impl RegisteredNetworks {
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
            self.default_network = next_default;
            Some(DefaultChangedEvent { previous_default: old_default })
        } else {
            None
        }
    }

    fn apply(&mut self, update: PropertyUpdate) -> RegistryUpdateResult {
        match update {
            PropertyUpdate::LoseDefaultNetwork => {
                // Handle Starnix unsetting its default network.
                self.starnix_default = None;
                RegistryUpdateResult {
                    event: UpdateApplied::None,
                    default_changed: self.handle_default_network_update(),
                }
            }
            PropertyUpdate::ChangeNetwork(network_id, network_change) => match network_change {
                NetworkUpdate::Properties(event) => RegistryUpdateResult {
                    event: self.handle_changed_network(network_id, event),
                    default_changed: self.handle_default_network_update(),
                },
                NetworkUpdate::Remove => {
                    if self.starnix_default == Some(network_id) {
                        error!("Cannot remove the default delegated network. Update ignored.");
                        RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
                    } else if self.networks.remove(&network_id).is_some() {
                        // Elect fallback default network internally.
                        RegistryUpdateResult {
                            event: UpdateApplied::NetworkRemoved(network_id),
                            default_changed: self.handle_default_network_update(),
                        }
                    } else {
                        error!("Cannot remove a non-existent network. Update ignored.");
                        RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
                    }
                }
                NetworkUpdate::MakeDefault => {
                    match network_id {
                        // Fuchsia networks are always the default network when present. Netcfg
                        // does not use this API to set a Fuchsia network as the default.
                        NetworkId::Fuchsia(_) => {}
                        NetworkId::Delegated(_) => self.starnix_default = Some(network_id),
                    }
                    let default_changed = self.handle_default_network_update();
                    RegistryUpdateResult { event: UpdateApplied::None, default_changed }
                }
            },
            PropertyUpdate::UpdateDns(dns_servers) => {
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

    // Handle the `NetworkPropertiesChange` in a `PropertyUpdate`, determining
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
            marks: socket_marks,
            dns_servers: changed_dns_servers,
            connectivity_state,
            name,
            network_type,
        } = event;
        let entry = self.networks.entry(network_id);
        let result = match (added, &entry, network_id, socket_marks) {
            (true, Entry::Occupied(_), _, _) => Err("add already added network"),
            (false, Entry::Vacant(_), _, _) => Err("update a non-added network"),
            (_, _, NetworkId::Fuchsia(_), Some(_)) => Err("have a fuchsia network with marks"),
            (_, _, NetworkId::Delegated(_), None) => Err("have a delegated network without marks"),
            (_, _, NetworkId::Fuchsia(_), None) => Ok((
                NetworkProperties {
                    dns_servers: changed_dns_servers.unwrap_or_default(),
                    ..Default::default()
                },
                added,
            )),
            (_, entry, NetworkId::Delegated(_), Some(socket_marks)) => {
                let changed = if let Entry::Occupied(e) = entry {
                    e.get().get_marks() != Some(&socket_marks)
                } else {
                    true
                };
                Ok((
                    NetworkProperties {
                        socket_marks: Some(socket_marks),
                        dns_servers: changed_dns_servers.unwrap_or_default(),
                        ..Default::default()
                    },
                    changed,
                ))
            }
        };

        match result {
            Ok((mut properties, changed_marks)) => {
                properties.connectivity_state = connectivity_state;
                properties.network_type = network_type;
                properties.name = name.clone();
                let _ = entry.insert_entry(properties);
                UpdateApplied::NetworkChanged {
                    network_id,
                    added,
                    changed_marks,
                    name,
                    network_type,
                }
            }
            Err(e) => {
                error!("Cannot {e}. Update ignored.");
                UpdateApplied::None
            }
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

    fn maybe_respond(
        &self,
        network: &NetworkTokenContents,
        responder: NetworkPropertyResponder,
    ) -> Option<NetworkPropertyResponder> {
        let mut updates = Vec::new();
        updates.add_socket_marks(self, network, &responder);
        updates.add_dns(self, network, &responder);

        if updates.is_empty() {
            Some(responder)
        } else {
            if let Err(e) = responder.respond(Ok(&updates)) {
                warn!("Could not send to responder: {e}");
            }
            None
        }
    }
}

trait PropertyUpdates {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        responder: &NetworkPropertyResponder,
    );
    fn add_dns(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        responder: &NetworkPropertyResponder,
    );
}

impl PropertyUpdates for Vec<fnp_properties::PropertyUpdate> {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        responder: &NetworkPropertyResponder,
    ) {
        if !responder.watched_properties.contains(&fnp_properties::Property::SocketMarks) {
            return;
        }

        match network_registry.networks.get(&network.network_id) {
            Some(network) => {
                if let Some(socket_marks) = network.get_marks() {
                    self.push(fnp_properties::PropertyUpdate::SocketMarks(socket_marks.clone()));
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
        responder: &NetworkPropertyResponder,
    ) {
        if !responder.watched_properties.contains(&fnp_properties::Property::DnsConfiguration) {
            return;
        }

        let interface_id = network.network_id;
        self.push(fnp_properties::PropertyUpdate::DnsConfiguration(
            fnp_properties::DnsConfiguration {
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
                                            source_interface,
                                            ..
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
                                        fnet_name::Dhcpv6DnsServerSource {
                                            source_interface, ..
                                        },
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
            },
        ));
    }
}

/// An event representing the properties that changed for a network.
#[derive(Clone, Debug, Default)]
pub struct NetworkPropertiesChange {
    /// When true, this is a new network being added. Otherwise, this is an
    /// update to an existing network.
    pub added: bool,
    /// The new marks for the network.
    pub marks: Option<fnet::Marks>,
    /// If present, contains the new DNS servers for this network.
    pub dns_servers: Option<Vec<fnet_name::DnsServer_>>,
    /// The new connectivity state of the network.
    pub connectivity_state: Option<fnp_socketproxy::ConnectivityState>,
    /// The name of the network.
    pub name: Option<String>,
    /// The transport type of the network.
    pub network_type: Option<fnp_socketproxy::NetworkType>,
}

#[derive(Debug)]
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
        name: Option<String>,
        network_type: Option<fnp_socketproxy::NetworkType>,
    },

    /// Network was removed, contains the NetworkId of the removed network.
    NetworkRemoved(NetworkId),
}

#[derive(Debug)]
pub enum PropertyUpdate {
    LoseDefaultNetwork,
    ChangeNetwork(NetworkId, NetworkUpdate),
    UpdateDns(Vec<fnet_name::DnsServer_>),
}

impl PropertyUpdate {
    pub fn default_network_lost() -> Self {
        PropertyUpdate::LoseDefaultNetwork
    }

    pub fn dns(dns_servers: &DnsServers) -> Self {
        // TODO(https://fxbug.dev/477980011): Switch to deriving dns servers from
        // NetworkRegistry updates.
        PropertyUpdate::UpdateDns(dns_servers.consolidated_dns_servers())
    }
}

/// The result of a delegated network update.
///
/// Returned to the main event loop to propagate system-wide configuration
/// changes (such as DNS server updates) and notify active watchers.
#[derive(Debug, PartialEq)]
pub struct DelegatedNetworkUpdateResult {
    /// If present, contains the new consolidated DNS servers known by the
    /// network registry.
    pub dns_servers: Option<Vec<fnet_name::DnsServer_>>,
}

#[derive(Default)]
pub struct NetpolNetworksService {
    // The current generation
    current_generation: UpdateGeneration,
    // The last generation sent per connection
    generations_by_connection: UpdateGenerations,
    // Default Network Watchers
    default_network_responders:
        HashMap<ConnectionId, fnp_properties::NetworksWatchDefaultResponder>,
    tokens: token_registry::TokenRegistry<NetworkTokenContents>,
    // NetworkProperty Watchers
    property_responders: HashMap<ConnectionId, NetworkPropertyResponder>,
    // The networks known to the system
    network_registry: RegisteredNetworks,
    telemetry: Option<TelemetrySender>,
}

impl NetpolNetworksService {
    pub fn set_telemetry(&mut self, telemetry: TelemetrySender) {
        self.telemetry = Some(telemetry);
    }

    /// Returns the consolidated DNS servers from the Network Registry.
    pub fn consolidated_dns_servers(&self) -> Vec<fnet_name::DnsServer_> {
        self.network_registry.consolidated_dns_servers()
    }

    pub async fn handle_network_attributes_request(
        &mut self,
        id: ConnectionId,
        req: Result<fnp_properties::NetworksRequest, fidl::Error>,
    ) -> Result<(), anyhow::Error> {
        let req = req.context("network attributes request")?;
        match req {
            fnp_properties::NetworksRequest::WatchDefault { responder } => {
                match self.default_network_responders.entry(id) {
                    std::collections::hash_map::Entry::Occupied(_) => {
                        warn!(
                            "Only one call to fuchsia.net.policy.properties/Networks.WatchDefault \
                             may be active per connection"
                        );
                        responder
                            .control_handle()
                            .shutdown_with_epitaph(zx::Status::CONNECTION_ABORTED)
                    }
                    std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                        let network_id = if self
                            .generations_by_connection
                            .default_network(&id)
                            .unwrap_or_default()
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
                            responder.send(
                                fnp_properties::NetworksWatchDefaultResponse::Network(token),
                            )?;

                            if let Some(responder) = self.property_responders.remove(&id) {
                                let _: Option<_> = self.generations_by_connection.remove(&id);
                                let _: Result<(), fidl::Error> =
                                    responder.respond(Err(fnp_properties::WatchError::NetworkGone));
                            }
                        } else {
                            let _: &mut _ = vacant_entry.insert(responder);
                        }
                    }
                }
            }
            fnp_properties::NetworksRequest::WatchProperties {
                payload: fnp_properties::NetworksWatchPropertiesRequest { network, properties, .. },
                responder,
            } => match (network, properties) {
                (None, _) | (_, None) => {
                    responder.send(Err(fnp_properties::WatchError::MissingRequiredArgument))?
                }
                (Some(network), Some(properties)) => {
                    if properties.is_empty() {
                        responder.send(Err(fnp_properties::WatchError::NoProperties))?;
                    } else {
                        match self.property_responders.entry(id) {
                            std::collections::hash_map::Entry::Occupied(_) => {
                                warn!(
                                    "Only one call to \
                                    fuchsia.net.policy.properties/Networks.WatchProperties may be \
                                    active per connection"
                                );
                                responder
                                    .control_handle()
                                    .shutdown_with_epitaph(zx::Status::CONNECTION_ABORTED)
                            }
                            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                                match self.tokens.get_contents(&network) {
                                    Err(e) => {
                                        warn!("Unknown network token. ({network:?}: {e})");
                                        responder.send(Err(
                                            fnp_properties::WatchError::InvalidNetworkToken,
                                        ))?;
                                    }
                                    Ok(network_contents) => {
                                        let responder = NetworkPropertyResponder {
                                            token: network,
                                            watched_properties: properties,
                                            responder,
                                        };
                                        if self
                                            .generations_by_connection
                                            .properties(&id)
                                            .unwrap_or_default()
                                            < self.current_generation.properties
                                        {
                                            self.generations_by_connection
                                                .set_properties(id, self.current_generation);
                                            if let Some(responder) = self
                                                .network_registry
                                                .maybe_respond(&network_contents, responder)
                                            {
                                                let _: &mut NetworkPropertyResponder =
                                                    vacant_entry.insert(responder);
                                            }
                                        } else {
                                            let _: &mut NetworkPropertyResponder =
                                                vacant_entry.insert(responder);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            _ => {
                warn!("Received unexpected request {req:?}");
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
    pub async fn handle_delegated_networks_update(
        &mut self,
        update: Result<fnp_socketproxy::NetworkRegistryRequest, fidl::Error>,
    ) -> Result<DelegatedNetworkUpdateResult, anyhow::Error> {
        use fnp_socketproxy::{
            NetworkInfo, NetworkRegistryAddError, NetworkRegistryRemoveError,
            NetworkRegistryRequest, NetworkRegistrySetDefaultError, NetworkRegistryUpdateError,
        };

        let action_result = match update {
            Err(e) => {
                error!(
                    "Encountered error watching for delegated network \
                                    updates: {e:?}"
                );
                return Err(anyhow::anyhow!(e));
            }
            Ok(NetworkRegistryRequest::SetDefault { network_id, responder }) => {
                let update_result = match network_id {
                    fposix_socket::OptionalUint32::Value(interface_id) => {
                        match InterfaceId::try_from(interface_id) {
                            Ok(id) => {
                                let delegated_id = NetworkId::delegated(id);
                                self.update(PropertyUpdate::ChangeNetwork(
                                    delegated_id,
                                    NetworkUpdate::MakeDefault,
                                ))
                                .await;
                                Ok(())
                            }
                            Err(_) => Err(NetworkRegistrySetDefaultError::NotFound),
                        }
                    }
                    fposix_socket::OptionalUint32::Unset(_) => {
                        self.update(PropertyUpdate::default_network_lost()).await;
                        Ok(())
                    }
                };

                self.respond_to_delegated_network_update(
                    update_result,
                    |reply| responder.send(reply),
                    "failed to send SetDefault result",
                )
            }
            Ok(NetworkRegistryRequest::Add { network, responder }) => {
                let extracted_properties = (|| {
                    let raw_network_id =
                        network.network_id.ok_or(NetworkRegistryAddError::MissingNetworkId)?;
                    let network_id = InterfaceId::try_from(raw_network_id)
                        .map(|id| NetworkId::delegated(id))
                        .map_err(|_| NetworkRegistryAddError::MissingNetworkId)?;
                    let NetworkInfo::Starnix(info) =
                        network.info.ok_or(NetworkRegistryAddError::MissingNetworkInfo)?
                    else {
                        return Err(NetworkRegistryAddError::MissingNetworkInfo);
                    };

                    let mut marks = fnet::Marks::default();
                    marks.set_mark(fnet::MARK_DOMAIN_SO_MARK, info.mark);

                    let dns_servers =
                        Self::extract_dns_servers(&network.dns_servers, raw_network_id.into());

                    Ok((network_id, marks, dns_servers))
                })();

                let update_result = match extracted_properties {
                    Ok((network_id, marks, dns_servers)) => {
                        self.update(PropertyUpdate::ChangeNetwork(
                            network_id,
                            NetworkUpdate::Properties(NetworkPropertiesChange {
                                added: true,
                                marks: Some(marks),
                                dns_servers: Some(dns_servers.clone()),
                                connectivity_state: network.connectivity,
                                name: network.name,
                                network_type: network.network_type,
                            }),
                        ))
                        .await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                };

                self.respond_to_delegated_network_update(
                    update_result,
                    |reply| responder.send(reply),
                    "failed to send Add result",
                )
            }
            Ok(NetworkRegistryRequest::Update { network, responder }) => {
                let extracted_properties = (|| {
                    let raw_network_id =
                        network.network_id.ok_or(NetworkRegistryUpdateError::MissingNetworkId)?;
                    let network_id = InterfaceId::try_from(raw_network_id)
                        .map(|id| NetworkId::delegated(id))
                        .map_err(|_| NetworkRegistryUpdateError::MissingNetworkId)?;
                    let NetworkInfo::Starnix(info) =
                        network.info.ok_or(NetworkRegistryUpdateError::MissingNetworkInfo)?
                    else {
                        return Err(NetworkRegistryUpdateError::MissingNetworkInfo);
                    };

                    let mut marks = fnet::Marks::default();
                    marks.set_mark(fnet::MARK_DOMAIN_SO_MARK, info.mark);

                    let dns_servers =
                        Self::extract_dns_servers(&network.dns_servers, raw_network_id.into());

                    Ok((network_id, marks, dns_servers))
                })();

                let update_result = match extracted_properties {
                    Ok((network_id, marks, dns_servers)) => {
                        self.update(PropertyUpdate::ChangeNetwork(
                            network_id,
                            NetworkUpdate::Properties(NetworkPropertiesChange {
                                added: false,
                                marks: Some(marks),
                                dns_servers: Some(dns_servers.clone()),
                                connectivity_state: network.connectivity,
                                name: network.name,
                                network_type: network.network_type,
                            }),
                        ))
                        .await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                };

                self.respond_to_delegated_network_update(
                    update_result,
                    |reply| responder.send(reply),
                    "failed to send Update result",
                )
            }
            Ok(NetworkRegistryRequest::Remove { network_id, responder }) => {
                let update_result = match InterfaceId::try_from(network_id) {
                    Ok(id) => {
                        let delegated_id = NetworkId::delegated(id);
                        self.update(PropertyUpdate::ChangeNetwork(
                            delegated_id,
                            NetworkUpdate::Remove,
                        ))
                        .await;
                        Ok(())
                    }
                    Err(_) => Err(NetworkRegistryRemoveError::NotFound),
                };

                self.respond_to_delegated_network_update(
                    update_result,
                    |reply| responder.send(reply),
                    "failed to send Remove result",
                )
            }
        };

        Ok(action_result)
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
        dns_servers: &Option<fnp_socketproxy::NetworkDnsServers>,
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
        responders: &mut HashMap<ConnectionId, NetworkPropertyResponder>,
    ) {
        let mut r = HashMap::new();
        std::mem::swap(&mut r, responders);
        r = r
            .into_iter()
            .filter_map(|(id, responder)| {
                match self.tokens.get_contents(&responder.token) {
                    Ok(contents) => {
                        // We only want to remove when watching a default token.
                        if contents.is_default {
                            let _: Option<_> = self.generations_by_connection.remove(&id);
                            let _: Result<(), fidl::Error> =
                                responder.respond(Err(fnp_properties::WatchError::NetworkGone));
                            return None;
                        }
                    }
                    Err(zx::Status::NOT_FOUND) => {
                        warn!("Token provided to get_contents is not valid.");
                    }
                    Err(e) => {
                        warn!("Encountered unknown issue while getting contents: {e}");
                    }
                }
                Some((id, responder))
            })
            .collect::<HashMap<_, _>>();
        std::mem::swap(&mut r, responders);
        self.tokens.drop_if(|&c| {
            c.is_default && previous_default_network.is_some_and(|i| i == c.network_id)
        });
    }

    pub(crate) async fn remove_network(&mut self, network_id: NetworkId) {
        info!("Removing interface {network_id}. Reporting NETWORK_GONE to all clients.");
        let mut responders = HashMap::new();
        std::mem::swap(&mut self.property_responders, &mut responders);
        for (id, responder) in responders {
            let network = match self.tokens.get_contents(&responder.token) {
                Ok(network) => network,
                Err(e) => {
                    warn!("Could not fetch network data for responder: {e}");
                    continue;
                }
            };
            if network.network_id == network_id {
                // Report that this interface was removed
                if let Err(e) = responder.respond(Err(fnp_properties::WatchError::NetworkGone)) {
                    warn!("Could not send to responder: {e}");
                }
            } else {
                if self.property_responders.insert(id, responder).is_some() {
                    error!("Re-inserted in an existing responder slot. This should be impossible.");
                }
            }
        }
    }

    pub async fn update(&mut self, update: PropertyUpdate) {
        self.current_generation.properties += 1;
        let RegistryUpdateResult { event, default_changed } = self.network_registry.apply(update);

        if let UpdateApplied::None = event {
            if default_changed.is_none() {
                // Return early if there were absolutely no changes and the default stayed the same.
                return;
            }
        }

        let mut property_responders = HashMap::new();
        std::mem::swap(&mut self.property_responders, &mut property_responders);

        // Clean up or register tokens based on whether the network was added or removed.
        match event {
            UpdateApplied::NetworkChanged { network_id, added: true, .. } => {
                let _ = self
                    .tokens
                    .ensure_token(NetworkTokenContents { network_id, is_default: false });
            }
            UpdateApplied::NetworkRemoved(network_id) => {
                self.tokens.drop_if(|c| !c.is_default && c.network_id == network_id);
            }
            UpdateApplied::NetworkChanged { added: false, .. }
            | UpdateApplied::DnsChanged
            | UpdateApplied::None => {}
        }

        // Notify watchers of default network changes if one occurred.
        if let Some(DefaultChangedEvent { previous_default }) = default_changed {
            self.notify_default_network_changed(previous_default, &mut property_responders).await;
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
                            .unwrap_or(fnp_socketproxy::NetworkType::Unknown),
                        is_fuchsia_provisioned: matches!(network_id, NetworkId::Fuchsia(_)),
                        connectivity_state: props.connectivity_state,
                    }));
                }
            }
        }

        for (id, responder) in property_responders {
            let mut updates = Vec::new();
            let network = match self.tokens.get_contents(&responder.token) {
                Ok(network) => network,
                Err(e) => {
                    warn!("Could not fetch network data for responder: {e}");
                    continue;
                }
            };

            if let UpdateApplied::NetworkChanged { network_id, changed_marks: true, .. } = event {
                if network.network_id == network_id {
                    updates.add_socket_marks(&self.network_registry, &network, &responder);
                }
            }
            if let UpdateApplied::DnsChanged = event {
                updates.add_dns(&self.network_registry, &network, &responder);
            }

            self.generations_by_connection.set_properties(id, self.current_generation);
            if updates.is_empty() {
                if self.property_responders.insert(id, responder).is_some() {
                    warn!("Re-inserted in an existing responder slot. This should be impossible.");
                }
            } else {
                if let Err(e) = responder.respond(Ok(&updates)) {
                    warn!("Could not send to responder: {e}");
                }
            }
        }
    }

    async fn notify_default_network_changed(
        &mut self,
        old_default: Option<NetworkId>,
        property_responders: &mut HashMap<ConnectionId, NetworkPropertyResponder>,
    ) {
        self.changed_default_network(old_default, property_responders).await;
        match self.network_registry.default_network {
            Some(default_network) => {
                if let Some(telemetry) = &self.telemetry {
                    if let Some(props) = self.network_registry.networks.get(&default_network) {
                        telemetry.send(TelemetryEvent::DefaultNetworkChanged(
                            NetworkEventMetadata {
                                id: default_network.get().get(),
                                name: props.name.clone(),
                                transport: props
                                    .network_type
                                    .unwrap_or(fnp_socketproxy::NetworkType::Unknown),
                                is_fuchsia_provisioned: matches!(
                                    default_network,
                                    NetworkId::Fuchsia(_)
                                ),
                                connectivity_state: props.connectivity_state,
                            },
                        ));
                    } else {
                        warn!("Could not fetch network data for default network.");
                    }
                }
                self.current_generation.default_network += 1;
                let mut responders = HashMap::new();
                std::mem::swap(&mut self.default_network_responders, &mut responders);
                for (id, responder) in responders {
                    self.generations_by_connection.set_default_network(id, self.current_generation);
                    match self
                        .tokens
                        .ensure_token(NetworkTokenContents {
                            network_id: default_network,
                            is_default: true,
                        })
                        .get()
                        .duplicate()
                    {
                        Ok(token) => {
                            if let Err(e) = responder
                                .send(fnp_properties::NetworksWatchDefaultResponse::Network(token))
                            {
                                warn!("Could not send to responder: {e}");
                            }
                        }
                        Err(e) => warn!("Could not duplicate token: {e}"),
                    };
                }
            }
            None => {
                if let Some(telemetry) = &self.telemetry {
                    telemetry.send(TelemetryEvent::DefaultNetworkLost);
                }
                // The default network has been lost.
                self.current_generation.default_network += 1;
                let mut responders = HashMap::new();
                std::mem::swap(&mut self.default_network_responders, &mut responders);
                for (id, responder) in responders {
                    self.generations_by_connection.set_default_network(id, self.current_generation);
                    if let Err(e) = responder.send(
                        fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(
                            fnp_properties::Empty,
                        ),
                    ) {
                        warn!("Could not send to responder: {e}");
                    }
                }
            }
        }
    }
}

pub struct ConnectionTagged<Stream: futures::Stream + Unpin> {
    next_id: ConnectionId,
    streams: futures::stream::SelectAll<Tagged<ConnectionId, Stream>>,
}

impl<Stream: futures::Stream + Unpin> Default for ConnectionTagged<Stream> {
    fn default() -> Self {
        Self { next_id: Default::default(), streams: Default::default() }
    }
}

impl<Stream: futures::Stream + Unpin> ConnectionTagged<Stream> {
    pub fn push(&mut self, stream: Stream) {
        self.streams.push(stream.tagged(self.next_id));
        self.next_id.0 += 1;
    }
}

impl<Stream: futures::Stream + Unpin> futures::Stream for ConnectionTagged<Stream> {
    type Item = (ConnectionId, <Stream as futures::Stream>::Item);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.streams).poll_next(cx)
    }
}

impl<Stream: futures::Stream + Unpin> futures::stream::FusedStream for ConnectionTagged<Stream> {
    fn is_terminated(&self) -> bool {
        self.streams.is_terminated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    const ID_1: InterfaceId = InterfaceId(NonZeroU64::new(1).unwrap());
    const ID_2: InterfaceId = InterfaceId(NonZeroU64::new(2).unwrap());
    const NAME_1: &str = "testif1";
    const NAME_2: &str = "testif2";

    impl NetpolNetworksService {
        pub(crate) fn default_network(&self) -> Option<NetworkId> {
            self.network_registry.default_network
        }

        pub(crate) fn has_network(&self, id: NetworkId) -> bool {
            self.network_registry.networks.contains_key(&id)
        }
    }

    #[test]
    fn test_handle_changed_network_delegated() {
        let mut networks = RegisteredNetworks::default();
        let delegated_id = NetworkId::Delegated(ID_1);

        // Add a new delegated network
        let marks = fnet::Marks { mark_1: Some(123), ..Default::default() };
        let event = NetworkPropertiesChange {
            added: true,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: Some(fnp_socketproxy::ConnectivityState::FullConnectivity),
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(
            networks.handle_changed_network(delegated_id, event),
            UpdateApplied::NetworkChanged {
                network_id: delegated_id,
                added: true,
                changed_marks: true,
                name: Some(NAME_1.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
            }
        );
        let properties = networks.networks.get(&delegated_id).expect("network should be present");
        assert_eq!(properties.socket_marks, Some(marks.clone()));
        assert_eq!(
            properties.connectivity_state,
            Some(fnp_socketproxy::ConnectivityState::FullConnectivity)
        );

        // Update with different connectivity state but same marks
        let event = NetworkPropertiesChange {
            added: false,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(
            networks.handle_changed_network(delegated_id, event),
            UpdateApplied::NetworkChanged {
                network_id: delegated_id,
                added: false,
                changed_marks: false,
                name: Some(NAME_1.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
            }
        );

        let properties = networks.networks.get(&delegated_id).expect("network should be present");
        assert_eq!(properties.socket_marks, Some(marks.clone()));
        assert_eq!(
            properties.connectivity_state,
            Some(fnp_socketproxy::ConnectivityState::NoConnectivity)
        );

        // Update with different marks
        let new_marks = fnet::Marks { mark_1: Some(456), ..Default::default() };
        let event = NetworkPropertiesChange {
            added: false,
            marks: Some(new_marks.clone()),
            dns_servers: None,
            connectivity_state: Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(
            networks.handle_changed_network(delegated_id, event),
            UpdateApplied::NetworkChanged {
                network_id: delegated_id,
                added: false,
                changed_marks: true,
                name: Some(NAME_1.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
            }
        );
        let properties = networks.networks.get(&delegated_id).expect("network should be present");
        assert_eq!(properties.socket_marks, Some(new_marks));
        assert_eq!(
            properties.connectivity_state,
            Some(fnp_socketproxy::ConnectivityState::NoConnectivity)
        );
    }

    #[test]
    fn test_handle_changed_network_fuchsia() {
        let mut networks = RegisteredNetworks::default();
        let fuchsia_id = NetworkId::Fuchsia(ID_2);

        // Add a Fuchsia network
        let event = NetworkPropertiesChange {
            added: true,
            marks: None,
            dns_servers: None,
            connectivity_state: Some(fnp_socketproxy::ConnectivityState::LocalConnectivity),
            name: Some(NAME_2.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Wifi),
        };
        assert_eq!(
            networks.handle_changed_network(fuchsia_id, event),
            UpdateApplied::NetworkChanged {
                network_id: fuchsia_id,
                added: true,
                changed_marks: true,
                name: Some(NAME_2.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Wifi),
            }
        );
        let properties = networks.networks.get(&fuchsia_id).expect("network should be present");
        assert_eq!(properties.socket_marks, None);
        assert_eq!(
            properties.connectivity_state,
            Some(fnp_socketproxy::ConnectivityState::LocalConnectivity)
        );

        // Update Fuchsia network connectivity
        let event = NetworkPropertiesChange {
            added: false,
            marks: None,
            dns_servers: None,
            connectivity_state: Some(fnp_socketproxy::ConnectivityState::FullConnectivity),
            name: Some(NAME_2.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Wifi),
        };
        assert_eq!(
            networks.handle_changed_network(fuchsia_id, event),
            UpdateApplied::NetworkChanged {
                network_id: fuchsia_id,
                added: false,
                changed_marks: false,
                name: Some(NAME_2.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Wifi),
            }
        );

        let properties = networks.networks.get(&fuchsia_id).expect("network should be present");
        assert_eq!(
            properties.connectivity_state,
            Some(fnp_socketproxy::ConnectivityState::FullConnectivity)
        );
    }

    #[test]
    fn test_handle_changed_network_validation() {
        let mut networks = RegisteredNetworks::default();
        let fuchsia_id = NetworkId::Fuchsia(ID_1);
        let network_id = NetworkId::Delegated(ID_1);
        let marks = fnet::Marks { mark_1: Some(123), ..Default::default() };

        // Update a non-added network
        let event = NetworkPropertiesChange {
            added: false,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: None,
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(networks.handle_changed_network(network_id, event), UpdateApplied::None);

        // Add the network
        let event = NetworkPropertiesChange {
            added: true,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: None,
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(
            networks.handle_changed_network(network_id, event),
            UpdateApplied::NetworkChanged {
                network_id,
                added: true,
                changed_marks: true,
                name: Some(NAME_1.to_string()),
                network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
            }
        );

        // Add already added network
        let event = NetworkPropertiesChange {
            added: true,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: None,
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(networks.handle_changed_network(network_id, event), UpdateApplied::None);

        // Fuchsia network with marks
        let event = NetworkPropertiesChange {
            added: true,
            marks: Some(marks.clone()),
            dns_servers: None,
            connectivity_state: None,
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(networks.handle_changed_network(fuchsia_id, event), UpdateApplied::None);

        // Delegated network without marks
        let delegated_id = NetworkId::Delegated(ID_1);
        let event = NetworkPropertiesChange {
            added: true,
            marks: None,
            dns_servers: None,
            connectivity_state: None,
            name: Some(NAME_1.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
        };
        assert_eq!(networks.handle_changed_network(delegated_id, event), UpdateApplied::None);

        // Make the network default
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(network_id, NetworkUpdate::MakeDefault)),
            RegistryUpdateResult {
                event: UpdateApplied::None,
                default_changed: Some(DefaultChangedEvent { previous_default: None })
            }
        );

        // Attempt to remove default network. This is invalid change since
        // the default must be unset prior to removal.
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(network_id, NetworkUpdate::Remove)),
            RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
        );

        // Verify it was not removed
        assert!(networks.networks.contains_key(&network_id));
        assert_eq!(networks.default_network, Some(network_id));
    }

    #[test]
    fn test_remove_fuchsia_network_fallback() {
        let mut networks = RegisteredNetworks::default();
        let fuchsia_id1 = NetworkId::Fuchsia(ID_1);
        let fuchsia_id2 = NetworkId::Fuchsia(ID_2);
        let delegated_id = NetworkId::Delegated(ID_1);

        let marks = fnet::Marks { mark_1: Some(123), ..Default::default() };

        // Add two Fuchsia networks and one Delegated network to the registry.
        let fuchsia_added_network_change =
            NetworkPropertiesChange { added: true, ..Default::default() };
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(
                fuchsia_id1,
                NetworkUpdate::Properties(fuchsia_added_network_change.clone())
            )),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkChanged {
                    network_id: fuchsia_id1,
                    added: true,
                    changed_marks: true,
                    name: None,
                    network_type: None,
                },
                default_changed: Some(DefaultChangedEvent { previous_default: None })
            }
        );
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(
                fuchsia_id2,
                NetworkUpdate::Properties(fuchsia_added_network_change)
            )),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkChanged {
                    network_id: fuchsia_id2,
                    added: true,
                    changed_marks: true,
                    name: None,
                    network_type: None,
                },
                default_changed: None
            }
        );
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(
                delegated_id,
                NetworkUpdate::Properties(NetworkPropertiesChange {
                    added: true,
                    marks: Some(marks),
                    ..Default::default()
                })
            )),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkChanged {
                    network_id: delegated_id,
                    added: true,
                    changed_marks: true,
                    name: None,
                    network_type: None,
                },
                default_changed: None
            }
        );

        // Make the Delegated network default. Since Fuchsia networks are
        // present, they are prioritized, so the active default network does
        // not change.
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(delegated_id, NetworkUpdate::MakeDefault)),
            RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
        );

        // Verify the active default is Fuchsia's network with the lowest ID.
        assert_eq!(networks.default_network, Some(fuchsia_id1));

        // Remove the Fuchsia active default network directly (allowed
        // statelessly for Fuchsia networks)
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(fuchsia_id1, NetworkUpdate::Remove)),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkRemoved(fuchsia_id1),
                default_changed: Some(DefaultChangedEvent { previous_default: Some(fuchsia_id1) })
            }
        );

        // Verify the fallback default is the next available Fuchsia network.
        assert_eq!(networks.default_network, Some(fuchsia_id2));

        // Remove the fallback Fuchsia network.
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(fuchsia_id2, NetworkUpdate::Remove)),
            RegistryUpdateResult {
                event: UpdateApplied::NetworkRemoved(fuchsia_id2),
                default_changed: Some(DefaultChangedEvent { previous_default: Some(fuchsia_id2) })
            }
        );

        // Verify the fallback default is the Delegated network since there are
        // no Fuchsia networks left.
        assert_eq!(networks.default_network, Some(delegated_id));

        // Remove the Delegated network directly. This must be rejected because
        // it is the Starnix default.
        assert_eq!(
            networks.apply(PropertyUpdate::ChangeNetwork(delegated_id, NetworkUpdate::Remove)),
            RegistryUpdateResult { event: UpdateApplied::None, default_changed: None }
        );
        assert!(networks.networks.contains_key(&delegated_id));
        assert_eq!(networks.default_network, Some(delegated_id));
    }
}
