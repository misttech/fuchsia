// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InterfaceId;
use crate::dns::DNS_PORT;
use crate::telemetry::{NetworkEventMetadata, TelemetryEvent, TelemetrySender};
use anyhow::Context as _;
use assert_matches::assert_matches;
use async_utils::stream::{Tagged, WithTag as _};
use dns_server_watcher::DnsServers;
use fidl::endpoints::{ControlHandle as _, Responder as _};
use futures::StreamExt as _;
use log::{debug, error, info, warn};
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

/// A unique identifier for a `fuchsia.net.policy.properties.WatchDefault` client connection.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(usize);

impl ConnectionId {
    fn increment(&mut self) {
        self.0 += 1;
    }
}

/// A unique identifier for a `fuchsia.net.policy.properties.PropertyWatcher` client connection.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyWatcherConnectionId(usize);

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
    default_network: HashMap<ConnectionId, usize>,
    properties: HashMap<PropertyWatcherConnectionId, usize>,
}

impl UpdateGenerations {
    fn default_network(&self, id: &ConnectionId) -> Option<usize> {
        self.default_network.get(id).copied()
    }

    fn set_default_network(&mut self, id: ConnectionId, generation: UpdateGeneration) {
        *self.default_network.entry(id).or_default() = generation.default_network;
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
}

#[derive(Debug, PartialEq, Default, Clone)]
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

    fn apply(&mut self, update: NetworkRegistryUpdate) -> RegistryUpdateResult {
        match update {
            NetworkRegistryUpdate::LoseDefaultNetwork => {
                // Handle Starnix unsetting its default network.
                self.starnix_default = None;
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
                        } else if self.networks.remove(&network_id).is_some() {
                            // Elect fallback default network internally.
                            RegistryUpdateResult {
                                event: UpdateApplied::NetworkRemoved(network_id),
                                default_changed: self.handle_default_network_update(),
                            }
                        } else {
                            error!("Cannot remove a non-existent network. Update ignored.");
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
                            NetworkId::Delegated(_) => self.starnix_default = Some(network_id),
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
            (_, entry, NetworkId::Fuchsia(_), None) => {
                let new_dns = changed_dns_servers.unwrap_or_default();
                let changed_dns = match entry {
                    Entry::Occupied(e) => e.get().dns_servers != new_dns,
                    // When adding a new network, set `changed_dns` to true so responders receive
                    // an explicit initial state.
                    Entry::Vacant(_) => true,
                };
                Ok((
                    NetworkProperties { dns_servers: new_dns, ..Default::default() },
                    added,
                    changed_dns,
                ))
            }
            (_, entry, NetworkId::Delegated(_), Some(socket_marks)) => {
                let new_dns = changed_dns_servers.unwrap_or_default();
                let (changed_marks, changed_dns) = match entry {
                    Entry::Occupied(e) => {
                        (e.get().get_marks() != Some(&socket_marks), e.get().dns_servers != new_dns)
                    }
                    // When adding a new network, set both `changed_marks` and `changed_dns` to
                    // true so responders receive an explicit initial state.
                    Entry::Vacant(_) => (true, true),
                };
                Ok((
                    NetworkProperties {
                        socket_marks: Some(socket_marks),
                        dns_servers: new_dns,
                        ..Default::default()
                    },
                    changed_marks,
                    changed_dns,
                ))
            }
        };

        match result {
            Ok((mut properties, changed_marks, changed_dns)) => {
                properties.connectivity_state = connectivity_state;
                properties.network_type = network_type;
                properties.name = name.clone();
                let _ = entry.insert_entry(properties);
                UpdateApplied::NetworkChanged {
                    network_id,
                    added,
                    changed_marks,
                    changed_dns,
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
}

/// Helper trait for building property update lists based on a client's registration.
trait PropertyUpdates {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        registration: &Registration,
    );
    fn add_dns(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        registration: &Registration,
    );
}

impl PropertyUpdates for fnp_properties::PropertyUpdate {
    fn add_socket_marks(
        &mut self,
        network_registry: &RegisteredNetworks,
        network: &NetworkTokenContents,
        registration: &Registration,
    ) {
        if !registration.properties.contains(fnp_properties::PropertyInterest::SOCKET_MARKS) {
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
        registration: &Registration,
    ) {
        if !registration.properties.contains(fnp_properties::PropertyInterest::DNS_CONFIGURATION) {
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
        network_type: Option<fnp_socketproxy::NetworkType>,
    },

    /// Network was removed, contains the NetworkId of the removed network.
    NetworkRemoved(NetworkId),
}

#[derive(Debug, Clone)]
pub enum NetworkRegistryUpdate {
    LoseDefaultNetwork,
    ChangeNetwork(NetworkId, NetworkUpdate),
    UpdateDns(Vec<fnet_name::DnsServer_>),
}

impl NetworkRegistryUpdate {
    pub fn default_network_lost() -> Self {
        NetworkRegistryUpdate::LoseDefaultNetwork
    }

    pub fn dns(dns_servers: &DnsServers) -> Self {
        // TODO(https://fxbug.dev/477980011): Switch to deriving dns servers from
        // NetworkRegistry updates.
        NetworkRegistryUpdate::UpdateDns(dns_servers.consolidated_dns_servers())
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
    PropertyWatcher {
        connection_id: PropertyWatcherConnectionId,
        stream: fnp_properties::PropertyWatcherRequestStream,
    },
    DelegatedNetworks(fnp_socketproxy::NetworkRegistryRequestStream),
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

impl From<fnp_socketproxy::NetworkRegistryRequestStream> for NetworkRequestStream {
    fn from(s: fnp_socketproxy::NetworkRegistryRequestStream) -> Self {
        Self::DelegatedNetworks(s)
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

/// An internal wrapper enum for active FIDL request streams stored in
/// [`NetpolNetworksService::streams`].
enum NetworkRequestStreamInner {
    Networks(Tagged<ConnectionId, fnp_properties::NetworksRequestStream>),
    NetworkTokenResolver(fnp_properties::NetworkTokenResolverRequestStream),
    PropertyWatcher(
        Tagged<PropertyWatcherConnectionId, fnp_properties::PropertyWatcherRequestStream>,
    ),
    DelegatedNetworks(fnp_socketproxy::NetworkRegistryRequestStream),
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
                    o.map(|(id, request)| NetworkAttributesRequest { id, request })
                        .map(NetworkRequest::NetworkAttributes)
                })
            }
            NetworkRequestStreamInner::NetworkTokenResolver(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|request| NetworkTokenResolverRequest { request })
                        .map(NetworkRequest::NetworkTokenResolver)
                })
            }
            NetworkRequestStreamInner::PropertyWatcher(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|(id, request)| PropertyWatcherRequest { id, request })
                        .map(NetworkRequest::PropertyWatcher)
                })
            }
            NetworkRequestStreamInner::DelegatedNetworks(ref mut stream) => {
                stream.poll_next_unpin(cx).map(|o| {
                    o.map(|request| DelegatedNetworksRequest { request })
                        .map(NetworkRequest::DelegatedNetworks)
                })
            }
        }
    }
}

/// A wrapper for [`fnp_properties::NetworksRequest`] that includes the [`ConnectionId`] of the
/// connection that sent the request.
pub struct NetworkAttributesRequest {
    pub id: ConnectionId,
    pub request: Result<fnp_properties::NetworksRequest, fidl::Error>,
}

/// A wrapper for [`fnp_properties::NetworkTokenResolverRequest`].
pub struct NetworkTokenResolverRequest {
    pub request: Result<fnp_properties::NetworkTokenResolverRequest, fidl::Error>,
}

/// A wrapper for [`fnp_properties::PropertyWatcherRequest`] that includes the [`ConnectionId`] of
/// the connection that sent the request.
pub struct PropertyWatcherRequest {
    pub id: PropertyWatcherConnectionId,
    pub request: Result<fnp_properties::PropertyWatcherRequest, fidl::Error>,
}

/// A wrapper for [`fnp_socketproxy::NetworkRegistryRequest`].
pub struct DelegatedNetworksRequest {
    pub request: Result<fnp_socketproxy::NetworkRegistryRequest, fidl::Error>,
}

/// An enum representing all possible events that can be received by the NetpolNetworksService
/// event loop.
pub enum NetworkRequest {
    NetworkAttributes(NetworkAttributesRequest),
    NetworkTokenResolver(NetworkTokenResolverRequest),
    PropertyWatcher(PropertyWatcherRequest),
    DelegatedNetworks(DelegatedNetworksRequest),
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
    property_watchers: HashMap<PropertyWatcherConnectionId, Registration>,
    // The networks known to the system
    network_registry: RegisteredNetworks,
    telemetry: Option<TelemetrySender>,
    // The next id to use for a networks connection
    next_networks_id: ConnectionId,
    // The next id to use for a property watcher connection
    next_watcher_id: PropertyWatcherConnectionId,
    // The multiplexed stream of events handled by the eventloop
    streams: futures::stream::SelectAll<NetworkRequestStreamInner>,
}

impl NetpolNetworksService {
    pub fn set_telemetry(&mut self, telemetry: TelemetrySender) {
        self.telemetry = Some(telemetry);
    }

    pub fn add_stream<S: Into<NetworkRequestStream>>(&mut self, s: S) {
        match s.into() {
            NetworkRequestStream::Networks(stream) => {
                self.streams.push(NetworkRequestStreamInner::Networks(
                    stream.tagged(self.next_networks_id),
                ));
                self.next_networks_id.increment();
            }
            NetworkRequestStream::NetworkTokenResolver(stream) => {
                self.streams.push(NetworkRequestStreamInner::NetworkTokenResolver(stream));
            }
            NetworkRequestStream::DelegatedNetworks(stream) => {
                self.streams.push(NetworkRequestStreamInner::DelegatedNetworks(stream));
            }
            NetworkRequestStream::PropertyWatcher { connection_id, stream } => {
                self.streams
                    .push(NetworkRequestStreamInner::PropertyWatcher(stream.tagged(connection_id)));
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
            NetworkRequest::DelegatedNetworks(DelegatedNetworksRequest { request }) => {
                self.handle_delegated_networks_update(request).await
            }
            NetworkRequest::PropertyWatcher(PropertyWatcherRequest { id, request }) => {
                self.handle_property_watcher_request(id, request).await?;
                Ok(DelegatedNetworkUpdateResult { dns_servers: None })
            }
        }
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
                        responder.control_handle().shutdown_with_epitaph(zx::Status::ALREADY_EXISTS)
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
                        } else {
                            let _: &mut _ = vacant_entry.insert(responder);
                        }
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
                                let watcher_id = self.next_watcher_id;
                                self.next_watcher_id.0 += 1;
                                let registration =
                                    Registration { token: network, properties, responder: None };
                                assert_matches!(
                                    self.property_watchers.insert(watcher_id, registration),
                                    None
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

    pub async fn handle_property_watcher_request(
        &mut self,
        id: PropertyWatcherConnectionId,
        req: Result<fnp_properties::PropertyWatcherRequest, fidl::Error>,
    ) -> Result<(), anyhow::Error> {
        match req {
            Err(e) => {
                info!("Property watcher request stream ended: {e:?}");
                // The id may no longer be present in either of the HashMaps depending
                // on whether the network was removed before the client disconnected.
                let _: Option<_> = self.property_watchers.remove(&id);
                let _: Option<_> = self.generations_by_connection.remove_properties(&id);
            }
            Ok(fnp_properties::PropertyWatcherRequest::Watch { responder }) => {
                match self.property_watchers.get_mut(&id) {
                    None => {
                        warn!("Received PropertyWatcher request for non-existent registration");
                        responder.control_handle().shutdown_with_epitaph(zx::Status::INTERNAL);
                    }
                    Some(registration) => {
                        if registration.responder.is_some() {
                            warn!(
                                "Only one call to \
                                fuchsia.net.policy.properties/PropertyWatcher.Watch may be \
                                active per connection"
                            );
                            responder
                                .control_handle()
                                .shutdown_with_epitaph(zx::Status::ALREADY_EXISTS);
                        } else {
                            registration.responder = Some(responder);
                            match self.tokens.get_contents(&registration.token) {
                                Ok(network_contents) => {
                                    // Determine whether a new update is available
                                    // (last_sent_generation < current_generation)
                                    if self
                                        .generations_by_connection
                                        .properties(&id)
                                        .unwrap_or_default()
                                        < self.current_generation.properties
                                    {
                                        self.generations_by_connection
                                            .set_properties(id, self.current_generation);
                                        let mut updates = fnp_properties::PropertyUpdate::default();
                                        updates.add_socket_marks(
                                            &self.network_registry,
                                            &network_contents,
                                            registration,
                                        );
                                        updates.add_dns(
                                            &self.network_registry,
                                            &network_contents,
                                            registration,
                                        );
                                        if updates != fnp_properties::PropertyUpdate::default() {
                                            if let Some(responder) = registration.responder.take() {
                                                responder.send(Ok(&updates))?;
                                            }
                                        }
                                    }
                                }
                                // The network was already removed (and its token dropped) prior to
                                // this `Watch` call.
                                Err(zx::Status::NOT_FOUND) => {
                                    let _: Option<_> =
                                        self.generations_by_connection.remove_properties(&id);
                                    if let Some(responder) = registration.responder.take() {
                                        let control_handle = responder.control_handle().clone();
                                        if let Err(e) = responder.send(Err(
                                            fnp_properties::PropertyWatcherError::NetworkGone,
                                        )) {
                                            warn!("Could not send to responder: {e}");
                                        }
                                        control_handle.shutdown();
                                    }
                                }
                                Err(e) => {
                                    warn!("Unexpected error fetching token contents: {e}");
                                    let _: Option<_> =
                                        self.generations_by_connection.remove_properties(&id);
                                    if let Some(responder) = registration.responder.take() {
                                        let control_handle = responder.control_handle().clone();
                                        if let Err(e) = responder.send(Err(
                                            fnp_properties::PropertyWatcherError::NetworkGone,
                                        )) {
                                            warn!("Could not send to responder: {e}");
                                        }
                                        control_handle.shutdown();
                                    }
                                }
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
                                self.update(NetworkRegistryUpdate::ChangeNetwork(
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
                        self.update(NetworkRegistryUpdate::default_network_lost()).await;
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
                        self.update(NetworkRegistryUpdate::ChangeNetwork(
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
                        self.update(NetworkRegistryUpdate::ChangeNetwork(
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
                        self.update(NetworkRegistryUpdate::ChangeNetwork(
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
        property_watchers: &mut HashMap<PropertyWatcherConnectionId, Registration>,
    ) {
        for (id, registration) in property_watchers.iter_mut() {
            if let Ok(contents) = self.tokens.get_contents(&registration.token) {
                if contents.is_default {
                    let _: Option<_> = self.generations_by_connection.remove_properties(id);
                    if let Some(responder) = registration.responder.take() {
                        let control_handle = responder.control_handle().clone();
                        if let Err(e) =
                            responder.send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                        {
                            warn!("Could not send to responder: {e}");
                        }
                        control_handle.shutdown();
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
                                let control_handle = responder.control_handle().clone();
                                if let Err(e) = responder
                                    .send(Err(fnp_properties::PropertyWatcherError::NetworkGone))
                                {
                                    warn!("Could not send to responder: {e}");
                                }
                                control_handle.shutdown();
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
                                    &registration,
                                );
                            }
                            if changed_dns {
                                updates.add_dns(&self.network_registry, &network, &registration);
                            }
                        }
                    }
                    UpdateApplied::DnsChanged => {
                        updates.add_dns(&self.network_registry, &network, &registration);
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
    }

    async fn notify_default_network_changed(
        &mut self,
        old_default: Option<NetworkId>,
        property_watchers: &mut HashMap<PropertyWatcherConnectionId, Registration>,
    ) {
        self.changed_default_network(old_default, property_watchers).await;
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
                                is_fuchsia_provisioned: default_network.is_fuchsia(),
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
    use assert_matches::assert_matches;
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
        network_type: fnp_socketproxy::NetworkType,
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
            connectivity_state: Option<fnp_socketproxy::ConnectivityState>,
        ) -> NetworkPropertiesChange {
            NetworkPropertiesChange {
                added,
                marks,
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
        network_type: fnp_socketproxy::NetworkType::Ethernet,
    };
    const FUCHSIA_NET_1: TestNetwork = TestNetwork {
        id: FUCHSIA_ID_1,
        name: NAME_1,
        network_type: fnp_socketproxy::NetworkType::Ethernet,
    };
    const FUCHSIA_NET_2: TestNetwork = TestNetwork {
        id: FUCHSIA_ID_2,
        name: NAME_2,
        network_type: fnp_socketproxy::NetworkType::Wifi,
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
        NetworkProperties { socket_marks: Some(test_marks()), ..Default::default() }
    }

    fn fuchsia_properties() -> NetworkProperties {
        NetworkProperties::default()
    }

    fn added_properties(name: &str) -> NetworkPropertiesChange {
        NetworkPropertiesChange {
            added: true,
            marks: None,
            dns_servers: None,
            connectivity_state: None,
            name: Some(name.to_string()),
            network_type: Some(fnp_socketproxy::NetworkType::Ethernet),
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
            Some(fnp_socketproxy::ConnectivityState::FullConnectivity),
        );
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: true, changed_marks: true, changed_dns: true }),
        );
        assert_eq!(
            networks.networks.get(&net.id).expect("network should be present"),
            &NetworkProperties {
                socket_marks: initial_marks.clone(),
                dns_servers: dns1,
                connectivity_state: Some(fnp_socketproxy::ConnectivityState::FullConnectivity),
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
            Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
        );
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: false, changed_marks: false, changed_dns: true }),
        );
        assert_eq!(
            networks.networks.get(&net.id).expect("network should be present"),
            &NetworkProperties {
                socket_marks: initial_marks.clone(),
                dns_servers: dns2.clone(),
                connectivity_state: Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
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
            Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
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
                socket_marks: updated_marks,
                dns_servers: dns2.clone(),
                connectivity_state: Some(fnp_socketproxy::ConnectivityState::NoConnectivity),
                name: Some(net.name.to_string()),
                network_type: Some(net.network_type),
            }
        );
    }

    #[test]
    fn test_handle_changed_network_validation() {
        let mut networks = RegisteredNetworks::default();
        let marks = test_marks();
        let net = DELEGATED_NET_1;

        // Update a non-added network
        let event = NetworkPropertiesChange { marks: Some(marks.clone()), ..net.added(false) };
        assert_eq!(networks.handle_changed_network(net.id, event), UpdateApplied::None);

        // Add the network
        let event = NetworkPropertiesChange { marks: Some(marks.clone()), ..net.added(true) };
        assert_eq!(
            networks.handle_changed_network(net.id, event),
            net.applied(AppliedChanges { added: true, changed_marks: true, changed_dns: true }),
        );

        // Add already added network
        let event = NetworkPropertiesChange { marks: Some(marks.clone()), ..net.added(true) };
        assert_eq!(networks.handle_changed_network(net.id, event), UpdateApplied::None);

        // Fuchsia network with marks
        let fuchsia_net = FUCHSIA_NET_1;
        let event =
            NetworkPropertiesChange { marks: Some(marks.clone()), ..fuchsia_net.added(true) };
        assert_eq!(networks.handle_changed_network(fuchsia_net.id, event), UpdateApplied::None);

        // Delegated network without marks
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
            networks.apply(NetworkRegistryUpdate::LoseDefaultNetwork),
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
                marks: Some(marks),
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

        // Active watcher immediately observes NetworkGone on its in-flight call.
        assert_matches!(
            active_watch_fut2.await,
            Ok(Err(fnp_properties::PropertyWatcherError::NetworkGone))
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
        service.update(NetworkRegistryUpdate::default_network_lost()).await;

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
}
