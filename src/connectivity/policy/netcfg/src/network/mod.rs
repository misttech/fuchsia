// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::InterfaceId;
use anyhow::Context as _;
use async_utils::stream::{Tagged, WithTag as _};
use dns_server_watcher::DnsServers;
use fidl::endpoints::{ControlHandle as _, Responder as _};
use log::{error, info, warn};
use policy_properties::NetworkTokenExt as _;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

mod token_registry;

use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_name as fnet_name,
    fidl_fuchsia_net_policy_properties as fnp_properties,
    fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy,
    fidl_fuchsia_posix_socket as fposix_socket,
};

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
}

impl NetworkProperties {
    fn get_marks(&self) -> Option<&fnet::Marks> {
        self.socket_marks.as_ref()
    }
}

/// The current state of all networks sent to the NetworkRegistry.
#[derive(Default, Clone)]
struct RegisteredNetworks {
    default_network: Option<NetworkId>,
    networks: HashMap<NetworkId, NetworkProperties>,
    dns_servers: Vec<fnet_name::DnsServer_>,
}

impl RegisteredNetworks {
    fn apply(&mut self, update: PropertyUpdate) -> UpdateApplied {
        match update {
            PropertyUpdate::LoseDefaultNetwork => self.handle_default_network_update(None),
            PropertyUpdate::ChangeNetwork(network_id, network_change) => match network_change {
                NetworkChange::Change(event) => self.handle_changed_network(network_id, event),
                NetworkChange::Remove => UpdateApplied::NetworkRemoved(network_id),
                NetworkChange::MakeDefault => self.handle_default_network_update(Some(network_id)),
            },
            PropertyUpdate::UpdateDns(dns_servers) => {
                if self.dns_servers != dns_servers {
                    self.dns_servers = dns_servers;
                    UpdateApplied::DnsChanged
                } else {
                    UpdateApplied::None
                }
            }
        }
    }

    // Handle the `default_network` argument in a `PropertyUpdate`, determining
    // whether the network changed as a result of the update.
    //
    // Returns an `UpdateApplied::DefaultNetworkChanged` if the new default
    // network is different from the old one.
    fn handle_default_network_update(
        &mut self,
        new_default_network: Option<NetworkId>,
    ) -> UpdateApplied {
        // We do not need to send an update applied if the network stayed the same.
        if new_default_network == self.default_network {
            return UpdateApplied::None;
        }

        let old_default_network = self.default_network;
        self.default_network = new_default_network;
        return UpdateApplied::DefaultNetworkChanged(old_default_network);
    }

    // Handle the `PropertyChangeEvent` in a `PropertyUpdate`, determining
    // whether network properties changed as a result of the update.
    //
    // Returns an `UpdateApplied::NetworkChanged` event if this is a valid change.
    fn handle_changed_network(
        &mut self,
        network_id: NetworkId,
        event: PropertyChangeEvent,
    ) -> UpdateApplied {
        let PropertyChangeEvent { added, marks: socket_marks } = event;
        let entry = self.networks.entry(network_id);
        let result = match (added, &entry, network_id, socket_marks) {
            (true, Entry::Occupied(_), _, _) => Err("add already added network"),
            (false, Entry::Vacant(_), _, _) => Err("update a non-added network"),
            (_, _, NetworkId::Fuchsia(_), Some(_)) => Err("have a fuchsia network with marks"),
            (_, _, NetworkId::Delegated(_), None) => Err("have a delegated network without marks"),

            (_, _, NetworkId::Fuchsia(_), None) => Ok((NetworkProperties::default(), added)),
            (_, entry, NetworkId::Delegated(_), Some(socket_marks)) => {
                let changed = if let Entry::Occupied(e) = entry {
                    e.get().get_marks() != Some(&socket_marks)
                } else {
                    true
                };
                Ok((NetworkProperties { socket_marks: Some(socket_marks) }, changed))
            }
        };

        match result {
            Ok((properties, changed_marks)) => {
                let _ = entry.insert_entry(properties);
                UpdateApplied::NetworkChanged { network_id, added, changed_marks }
            }
            Err(e) => {
                error!("Cannot {e}. Update ignored.");
                UpdateApplied::None
            }
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

// TODO(https://fxbug.dev/483165646): Improve naming in this module.
/// An event representing the properties that changed for a network.
#[derive(Debug)]
pub struct PropertyChangeEvent {
    /// When true, this is a new network being added. Otherwise, this is an
    /// update to an existing network.
    pub added: bool,
    /// The new marks for the network.
    pub marks: Option<fnet::Marks>,
}

#[derive(Debug)]
pub enum NetworkChange {
    /// Change a network's properties.
    Change(PropertyChangeEvent),
    Remove,
    MakeDefault,
}

#[derive(Debug)]
enum UpdateApplied {
    /// No update was performed.
    None,

    /// A default network has changed. Carries the previous default id, if any.
    DefaultNetworkChanged(Option<NetworkId>),

    /// Whether the DNS servers changed.
    DnsChanged,

    /// Network was added or updated, contains the NetworkId of the added network.
    NetworkChanged { network_id: NetworkId, added: bool, changed_marks: bool },

    /// Network was removed, contains the NetworkId of the removed network.
    NetworkRemoved(NetworkId),
}

#[derive(Debug)]
pub enum PropertyUpdate {
    LoseDefaultNetwork,
    ChangeNetwork(NetworkId, NetworkChange),
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
}

impl NetpolNetworksService {
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

    pub async fn handle_delegated_networks_update(
        &mut self,
        update: Result<fnp_socketproxy::NetworkRegistryRequest, fidl::Error>,
    ) -> Result<(), anyhow::Error> {
        use fnp_socketproxy::{
            NetworkInfo, NetworkRegistryAddError, NetworkRegistryRemoveError,
            NetworkRegistryRequest, NetworkRegistrySetDefaultError, NetworkRegistryUpdateError,
        };

        match update {
            Err(e) => {
                error!(
                    "Encountered error watching for delegated network \
                                    updates: {e:?}"
                );
                Ok(())
            }
            Ok(NetworkRegistryRequest::SetDefault { network_id, responder }) => responder.send(
                (async || match network_id {
                    // TODO(https://fxbug.dev/475266563): Stop using
                    // `fuchsia.posix.socket.OptionalUint32` here.
                    fposix_socket::OptionalUint32::Value(interface_id) => {
                        self.update(PropertyUpdate::ChangeNetwork(
                            NetworkId::delegated(
                                InterfaceId::try_from(interface_id)
                                    .map_err(|_| NetworkRegistrySetDefaultError::NotFound)?,
                            ),
                            NetworkChange::MakeDefault,
                        ))
                        .await;
                        Ok(())
                    }
                    fposix_socket::OptionalUint32::Unset(_) => {
                        self.update(PropertyUpdate::default_network_lost()).await;
                        Ok(())
                    }
                })()
                .await,
            ),
            Ok(NetworkRegistryRequest::Add { network, responder }) => responder.send(
                (async || {
                    let network_id = network
                        .network_id
                        .and_then(|id| InterfaceId::try_from(id).ok())
                        .map(|id| NetworkId::delegated(id))
                        .ok_or(NetworkRegistryAddError::MissingNetworkId)?;
                    let NetworkInfo::Starnix(info) =
                        network.info.ok_or(NetworkRegistryAddError::MissingNetworkInfo)?
                    else {
                        return Err(NetworkRegistryAddError::MissingNetworkInfo);
                    };

                    // TODO(https://fxbug.dev/431822969): Replace this with a common definition
                    // of which mark domain is used for which purpose.
                    let marks =
                        Some(fnet::Marks { mark_1: info.mark, mark_2: None, ..Default::default() });

                    // TODO(https://fxbug.dev/477980011): Also include DNS update here,
                    // rather than relying on DnsServerWatcher provided by socket-proxy.
                    self.update(PropertyUpdate::ChangeNetwork(
                        network_id,
                        NetworkChange::Change(PropertyChangeEvent { added: true, marks }),
                    ))
                    .await;
                    Ok(())
                })()
                .await,
            ),
            Ok(NetworkRegistryRequest::Update { network, responder }) => responder.send(
                (async || {
                    let network_id = network
                        .network_id
                        .and_then(|id| InterfaceId::try_from(id).ok())
                        .map(|id| NetworkId::delegated(id))
                        .ok_or(NetworkRegistryUpdateError::MissingNetworkId)?;
                    let NetworkInfo::Starnix(info) =
                        network.info.ok_or(NetworkRegistryUpdateError::MissingNetworkInfo)?
                    else {
                        return Err(NetworkRegistryUpdateError::MissingNetworkInfo);
                    };

                    // TODO(https://fxbug.dev/431822969): Replace this with a common definition
                    // of which mark domain is used for which purpose.
                    let marks =
                        Some(fnet::Marks { mark_1: info.mark, mark_2: None, ..Default::default() });
                    self.update(PropertyUpdate::ChangeNetwork(
                        network_id,
                        NetworkChange::Change(PropertyChangeEvent { added: false, marks }),
                    ))
                    .await;
                    Ok(())
                })()
                .await,
            ),
            Ok(NetworkRegistryRequest::Remove { network_id, responder }) => responder.send(
                (async || {
                    self.update(PropertyUpdate::ChangeNetwork(
                        NetworkId::delegated(
                            // Try to convert network_id to an `InterfaceId`. If
                            // this fails (i.e. the network_id is 0) this is
                            // treated the same as a `NOT_FOUND` error.
                            InterfaceId::try_from(network_id)
                                .map_err(|_| NetworkRegistryRemoveError::NotFound)?,
                        ),
                        NetworkChange::Remove,
                    ))
                    .await;
                    Ok(())
                })()
                .await,
            ),
        }
        .context("while handling DelegatedNetwork request")
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
        let update_applied = self.network_registry.apply(update);
        if let UpdateApplied::None = update_applied {
            // Return early if the update resulted in no changes.
            return;
        }

        let mut property_responders = HashMap::new();
        std::mem::swap(&mut self.property_responders, &mut property_responders);

        match update_applied {
            UpdateApplied::DefaultNetworkChanged(previous_default) => {
                self.changed_default_network(previous_default, &mut property_responders).await;
                match self.network_registry.default_network {
                    Some(default_network) => {
                        self.current_generation.default_network += 1;
                        let mut responders = HashMap::new();
                        std::mem::swap(&mut self.default_network_responders, &mut responders);
                        for (id, responder) in responders {
                            self.generations_by_connection
                                .set_default_network(id, self.current_generation);
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
                                    if let Err(e) = responder.send(
                                        fnp_properties::NetworksWatchDefaultResponse::Network(
                                            token,
                                        ),
                                    ) {
                                        warn!("Could not send to responder: {e}");
                                    }
                                }
                                Err(e) => warn!("Could not duplicate token: {e}"),
                            };
                        }
                    }
                    None => {
                        // The default network has been lost.
                        self.current_generation.default_network += 1;
                        let mut responders = HashMap::new();
                        std::mem::swap(&mut self.default_network_responders, &mut responders);
                        for (id, responder) in responders {
                            self.generations_by_connection
                                .set_default_network(id, self.current_generation);
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

                // All property updaters have been notified
                return;
            }
            UpdateApplied::NetworkChanged { network_id, added: true, .. } => {
                let _ = self
                    .tokens
                    .ensure_token(NetworkTokenContents { network_id, is_default: false });
            }
            UpdateApplied::NetworkRemoved(network_id) => {
                self.tokens.drop_if(|c| !c.is_default && c.network_id == network_id);
            }
            UpdateApplied::NetworkChanged { added: false, .. } => {
                // The network already exists so the token must also exist.
                // No action is needed.
            }
            // TODO(https://fxbug.dev/477980011): Switch to deriving dns servers from
            // NetworkRegistry updates.
            UpdateApplied::DnsChanged => {}
            UpdateApplied::None => {}
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

            if let UpdateApplied::NetworkChanged { network_id, changed_marks: true, .. } =
                update_applied
            {
                if network.network_id == network_id {
                    updates.add_socket_marks(&self.network_registry, &network, &responder);
                }
            }
            if let UpdateApplied::DnsChanged = update_applied {
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
