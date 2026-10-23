// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, anyhow, format_err};
use async_helpers::hanging_get::asynchronous as hanging_get;
use fidl::endpoints::{Proxy, ServerEnd};
use fidl_fuchsia_bluetooth::{Appearance, DeviceClass};
use fidl_fuchsia_bluetooth_bredr::ProfileMarker;
use fidl_fuchsia_bluetooth_gatt::Server_Marker;
use fidl_fuchsia_bluetooth_gatt2::{
    LocalServiceRequest, Server_Marker as Server_Marker2, Server_Proxy,
};
use fidl_fuchsia_bluetooth_host::{DiscoverySessionProxy, HostProxy, ProtocolRequest};
use fidl_fuchsia_bluetooth_le::{CentralMarker, PeripheralMarker, PrivilegedPeripheralMarker};
use fidl_fuchsia_bluetooth_sys::{
    self as sys, InputCapability, OutputCapability, PairingDelegateProxy,
};
use fuchsia_async::{self as fasync, DurationExt, TimeoutExt};
use fuchsia_bluetooth::inspect::{DebugExt, Inspectable};
use fuchsia_bluetooth::types::pairing_options::PairingOptions;
use fuchsia_bluetooth::types::{
    Address, BondingData, HostData, HostId, HostInfo, Identity, Peer, PeerId,
};
use fuchsia_inspect::{self as inspect, NumericProperty, Property, unique_name};
use fuchsia_inspect_contrib::inspect_log;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_inspect_derive::Inspect;
use fuchsia_sync::RwLock;
use futures::FutureExt;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, BoxFuture, FusedFuture, Future, Shared};
use log::{debug, error, info, trace, warn};
use slab::Slab;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};
use zx::{AsHandleRef, MonotonicDuration};

use crate::host_device::{HostDevice, HostListener};
use crate::services::pairing::pairing_dispatcher::{PairingDispatcher, PairingDispatcherHandle};
use crate::store::stash::Stash;
use crate::watch_peers::PeerWatcher;
use crate::{build_config, generic_access_service, types};

pub use fidl_fuchsia_device::DEFAULT_DEVICE_NAME;

/// Policies for HostDispatcher::set_name
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum NameReplace {
    /// Keep the current name if it is already set, but set a new name if it hasn't been.
    Keep,
    /// Replace the current name unconditionally.
    Replace,
}

#[cfg(not(test))]
pub static HOST_INIT_TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(200);
// Speed up tests that wait for this timeout to go off.
#[cfg(test)]
pub static HOST_INIT_TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(10);

/// Available FIDL services that can be provided by a particular Host
#[derive(Copy, Clone)]
pub enum HostService {
    LeCentral,
    LePeripheral,
    LePrivilegedPeripheral,
    LeGatt,
    LeGatt2,
    Profile,
}

/// When a client requests Discovery, we establish and store two distinct sessions; the dispatcher
/// DiscoverySession, an Arc<> of which is returned to clients and represents the dispatcher's
/// state of discovery that perists as long as one client maintains an Arc<> to the session, and
/// the DiscoverySessionProxy, which is returned by the active host device on which discovery is
/// physically occurring and persists until the host disappears or discovery is stopped.
pub enum DiscoveryState {
    NotDiscovering,
    Pending {
        // Additional client requests for discovery made while discovery is asynchronously
        // starting.
        session_receiver: Shared<oneshot::Receiver<Arc<DiscoverySession>>>,
        session_sender: oneshot::Sender<Arc<DiscoverySession>>,
        discovery_stopped_receiver: Shared<oneshot::Receiver<()>>,
        discovery_stopped_sender: oneshot::Sender<()>,
        start_discovery_task: fasync::Task<()>,
    },
    Discovering {
        session: Weak<DiscoverySession>,
        discovery_proxy: DiscoverySessionProxy,
        started: fasync::MonotonicInstant,
        discovery_on_closed_task: fasync::Task<()>,
        discovery_stopped_receiver: Shared<oneshot::Receiver<()>>,
        discovery_stopped_sender: oneshot::Sender<()>,
    },
    Stopping {
        // DiscoverySessionProxy needs to be held while stopping so that peer closed signal can be received.
        discovery_proxy: DiscoverySessionProxy,
        // Contains requests for discovery made while discovery is stopping (edge case).
        // This field is optional so we know if we don't need to restart discovery.
        session_receiver: Option<Shared<oneshot::Receiver<Arc<DiscoverySession>>>>,
        session_sender: Option<oneshot::Sender<Arc<DiscoverySession>>>,
        discovery_on_closed_task: fasync::Task<()>,
        discovery_stopped_receiver: Shared<oneshot::Receiver<()>>,
        discovery_stopped_sender: oneshot::Sender<()>,
    },
}

impl DiscoveryState {
    // Idempotently end the discovery session.
    // Returns the duration of a session, if one was ended.
    fn stop_discovery_session(&mut self) -> Option<fasync::MonotonicDuration> {
        if let DiscoveryState::Discovering {
            discovery_proxy,
            started,
            discovery_on_closed_task,
            discovery_stopped_receiver,
            discovery_stopped_sender,
            ..
        } = std::mem::replace(self, DiscoveryState::NotDiscovering)
        {
            // stop() is synchronous, but stopping is an asynchronous procedure and will complete
            // when the Discovery event stream terminates.
            let _ = discovery_proxy.stop();
            *self = DiscoveryState::Stopping {
                discovery_proxy,
                session_receiver: None,
                session_sender: None,
                discovery_on_closed_task,
                discovery_stopped_receiver,
                discovery_stopped_sender,
            };
            return Some(fasync::MonotonicInstant::now() - started);
        }
        None
    }

    pub fn on_stopped(&mut self) -> impl FusedFuture<Output = ()> + use<> {
        let rx = match self {
            DiscoveryState::NotDiscovering => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(());
                rx.shared()
            }
            DiscoveryState::Pending { discovery_stopped_receiver, .. } => {
                discovery_stopped_receiver.clone()
            }
            DiscoveryState::Discovering { discovery_stopped_receiver, .. } => {
                discovery_stopped_receiver.clone()
            }
            DiscoveryState::Stopping { discovery_stopped_receiver, .. } => {
                discovery_stopped_receiver.clone()
            }
        };
        rx.map(|_| ())
    }

    fn get_active_session_future(
        &mut self,
    ) -> Option<BoxFuture<'static, Result<Arc<DiscoverySession>, oneshot::Canceled>>> {
        match self {
            DiscoveryState::NotDiscovering => None,
            DiscoveryState::Pending { session_receiver, .. } => {
                Some(session_receiver.clone().boxed())
            }
            DiscoveryState::Discovering { session, .. } => {
                let session = session.upgrade().expect("session must exist in Discovering state");
                Some(future::ready(Ok(session)).boxed())
            }
            DiscoveryState::Stopping { session_receiver, session_sender, .. } => {
                match session_receiver {
                    Some(recv) => Some(recv.clone().boxed()),
                    None => {
                        let (send, recv) = oneshot::channel();
                        let recv = recv.shared();
                        *session_receiver = Some(recv.clone());
                        *session_sender = Some(send);
                        Some(recv.boxed())
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for DiscoveryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryState::NotDiscovering => {
                write!(f, "DiscoveryState::NotDiscovering")
            }
            DiscoveryState::Pending { .. } => {
                write!(f, "DiscoveryState::Pending")
            }
            DiscoveryState::Discovering { .. } => {
                write!(f, "DiscoveryState::Discovering")
            }
            DiscoveryState::Stopping { .. } => {
                write!(f, "DiscoveryState::Stopping")
            }
        }
    }
}

/// A dispatcher discovery session, which persists as long as at least one client holds an
/// Arc<> to it.
pub struct DiscoverySession {
    dispatcher_state: Arc<RwLock<HostDispatcherState>>,
}

impl DiscoverySession {
    pub fn on_discovery_end(&self) -> impl FusedFuture<Output = ()> + use<> {
        self.dispatcher_state.write().discovery.on_stopped()
    }
}

impl Drop for DiscoverySession {
    fn drop(&mut self) {
        let mut write = self.dispatcher_state.write();
        if let Some(dur) = write.discovery.stop_discovery_session() {
            inspect_log!(write.inspect.discovery_history, duration: dur.into_seconds_f64());
        }
    }
}

pub enum SuppressConnectionsState {
    NotSuppressed,
    Pending {
        // Additional client requests for connection suppression made while suppression is
        // asynchronously starting.
        session_receiver: Shared<oneshot::Receiver<Arc<SuppressConnectionsSession>>>,
        session_sender: oneshot::Sender<Arc<SuppressConnectionsSession>>,
        _start_suppression_task: fasync::Task<()>,
    },
    Suppressed {
        session: Weak<SuppressConnectionsSession>,
        host_id: HostId,
    },
}

impl SuppressConnectionsState {
    // Idempotently end the suppression session if no active references remain.
    // Returns the HostId of the suppressed host if suppression was ended.
    fn stop_suppression_session(&mut self) -> Option<HostId> {
        if matches!(
            self,
            SuppressConnectionsState::Suppressed { session, .. } if session.strong_count() == 0
        ) {
            if let SuppressConnectionsState::Suppressed { host_id, .. } =
                std::mem::replace(self, SuppressConnectionsState::NotSuppressed)
            {
                return Some(host_id);
            }
        }
        None
    }

    fn set_suppressed(
        &mut self,
        host_id: HostId,
        session: &Arc<SuppressConnectionsSession>,
    ) -> Option<oneshot::Sender<Arc<SuppressConnectionsSession>>> {
        if let SuppressConnectionsState::Pending { session_sender, .. } =
            std::mem::replace(self, SuppressConnectionsState::NotSuppressed)
        {
            *self =
                SuppressConnectionsState::Suppressed { session: Arc::downgrade(session), host_id };
            Some(session_sender)
        } else {
            None
        }
    }

    fn get_active_session_future(
        &self,
    ) -> Option<BoxFuture<'static, Result<Arc<SuppressConnectionsSession>, oneshot::Canceled>>>
    {
        match self {
            SuppressConnectionsState::NotSuppressed => None,
            SuppressConnectionsState::Pending { session_receiver, .. } => {
                Some(session_receiver.clone().boxed())
            }
            SuppressConnectionsState::Suppressed { session, .. } => {
                session.upgrade().map(|s| future::ready(Ok(s)).boxed())
            }
        }
    }
}

impl std::fmt::Debug for SuppressConnectionsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SuppressConnectionsState::NotSuppressed => {
                write!(f, "SuppressConnectionsState::NotSuppressed")
            }
            SuppressConnectionsState::Pending { .. } => {
                write!(f, "SuppressConnectionsState::Pending")
            }
            SuppressConnectionsState::Suppressed { .. } => {
                write!(f, "SuppressConnectionsState::Suppressed")
            }
        }
    }
}

/// A dispatcher connection suppression session, which persists as long as at least one client
/// holds an Arc<> to it.
pub struct SuppressConnectionsSession {
    dispatcher_state: Arc<RwLock<HostDispatcherState>>,
}

impl Drop for SuppressConnectionsSession {
    fn drop(&mut self) {
        let mut write = self.dispatcher_state.write();
        if let Some(host_id) = write.suppress_connections.stop_suppression_session() {
            if let Some(host) = write.host_devices.get(&host_id).cloned() {
                let fut = host.set_connectable(true);
                fasync::Task::spawn(async move {
                    if let Err(e) = fut.await {
                        warn!("Unexpected error response when enabling connectable: {:?}", e);
                    }
                })
                .detach();
            }
        }
    }
}

enum DiscoverableState {
    NotDiscoverable,
    Pending {
        session: Weak<DiscoverableSession>,
        // Additional client requests for discoverable made while discoverable is
        // asynchronously starting.
        session_receiver: Shared<oneshot::Receiver<Arc<DiscoverableSession>>>,
        session_sender: oneshot::Sender<Arc<DiscoverableSession>>,
        _start_discoverable_task: fasync::Task<()>,
    },
    Discoverable {
        session: Weak<DiscoverableSession>,
        host_id: HostId,
    },
}

impl DiscoverableState {
    // Idempotently end the discoverable session if no active references remain.
    // Returns the HostId of the discoverable host if discoverable was ended.
    fn stop_discoverable_session(&mut self) -> Option<HostId> {
        if matches!(
            self,
            DiscoverableState::Discoverable { session, .. } if session.strong_count() == 0
        ) {
            if let DiscoverableState::Discoverable { host_id, .. } =
                std::mem::replace(self, DiscoverableState::NotDiscoverable)
            {
                return Some(host_id);
            }
        }
        None
    }

    fn clear_pending_if_matching(
        &mut self,
        target_session: &Arc<DiscoverableSession>,
    ) -> Option<DiscoverableState> {
        if matches!(
            self,
            DiscoverableState::Pending { session, .. }
                if std::ptr::eq(session.as_ptr(), Arc::as_ptr(target_session))
        ) {
            Some(std::mem::replace(self, DiscoverableState::NotDiscoverable))
        } else {
            None
        }
    }

    fn set_discoverable(
        &mut self,
        host_id: HostId,
        target_session: &Arc<DiscoverableSession>,
    ) -> Option<oneshot::Sender<Arc<DiscoverableSession>>> {
        if matches!(
            self,
            DiscoverableState::Pending { session, .. }
                if std::ptr::eq(session.as_ptr(), Arc::as_ptr(target_session))
        ) {
            if let DiscoverableState::Pending { session, session_sender, .. } =
                std::mem::replace(self, DiscoverableState::NotDiscoverable)
            {
                *self = DiscoverableState::Discoverable { session, host_id };
                return Some(session_sender);
            }
        }
        None
    }

    fn active_session_future(
        &self,
    ) -> Option<BoxFuture<'static, Result<Arc<DiscoverableSession>, oneshot::Canceled>>> {
        match self {
            DiscoverableState::NotDiscoverable => None,
            DiscoverableState::Pending { session_receiver, .. } => {
                Some(session_receiver.clone().boxed())
            }
            DiscoverableState::Discoverable { session, .. } => {
                session.upgrade().map(|s| future::ready(Ok(s)).boxed())
            }
        }
    }
}

impl std::fmt::Debug for DiscoverableState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverableState::NotDiscoverable => {
                write!(f, "DiscoverableState::NotDiscoverable")
            }
            DiscoverableState::Pending { .. } => {
                write!(f, "DiscoverableState::Pending")
            }
            DiscoverableState::Discoverable { .. } => {
                write!(f, "DiscoverableState::Discoverable")
            }
        }
    }
}

/// A dispatcher discoverable session, which persists as long as at least one client
/// holds an Arc<> to it.
pub(crate) struct DiscoverableSession {
    dispatcher_state: Arc<RwLock<HostDispatcherState>>,
}

impl std::fmt::Debug for DiscoverableSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoverableSession").finish_non_exhaustive()
    }
}

impl Drop for DiscoverableSession {
    fn drop(&mut self) {
        let mut write = self.dispatcher_state.write();
        if let Some(host_id) = write.discoverable.stop_discoverable_session() {
            if let Some(host) = write.host_devices.get(&host_id).cloned() {
                let fut = host.set_discoverable(false);
                fasync::Task::spawn(async move {
                    if let Err(e) = fut.await {
                        warn!("Unexpected error response when disabling discoverable: {:?}", e);
                    }
                })
                .detach();
            }
        }
    }
}

static RECENTLY_REMOVED_PEERS_COUNT: usize = 15;
static RECENT_DISCOVERY_SESSIONS_COUNT: usize = 5;

struct HostDispatcherInspect {
    _inspect: inspect::Node,
    peers: inspect::Node,
    hosts: inspect::Node,
    host_count: inspect::UintProperty,
    device_class: inspect::StringProperty,
    peer_count: inspect::UintProperty,
    evicted_peers: BoundedListNode,
    discovery_sessions: inspect::UintProperty,
    discovery_history: BoundedListNode,
}

impl HostDispatcherInspect {
    pub fn new(inspect: inspect::Node) -> HostDispatcherInspect {
        HostDispatcherInspect {
            host_count: inspect.create_uint("host_count", 0),
            peer_count: inspect.create_uint("peer_count", 0),
            device_class: inspect.create_string("device_class", "default"),
            peers: inspect.create_child("peers"),
            hosts: inspect.create_child("hosts"),
            discovery_sessions: inspect.create_uint("discovery_sessions", 0),
            discovery_history: BoundedListNode::new(
                inspect.create_child("discovery_history"),
                RECENT_DISCOVERY_SESSIONS_COUNT,
            ),
            evicted_peers: BoundedListNode::new(
                inspect.create_child("recently_removed"),
                RECENTLY_REMOVED_PEERS_COUNT,
            ),
            _inspect: inspect,
        }
    }

    pub fn peers(&self) -> &inspect::Node {
        &self.peers
    }

    pub fn hosts(&self) -> &inspect::Node {
        &self.hosts
    }
}

/// The HostDispatcher acts as a proxy aggregating multiple HostAdapters
/// It appears as a Host to higher level systems, and is responsible for
/// routing commands to the appropriate HostAdapter
struct HostDispatcherState {
    host_devices: HashMap<HostId, HostDevice>,
    initializing_hosts: HashMap<HostId, Address>,
    active_id: Option<HostId>,

    // Component storage.
    stash: Stash,

    // GAP state
    // Name, if set. If not set, hosts will not have a set name.
    name: Option<String>,
    appearance: Appearance,
    discovery: DiscoveryState,
    discoverable: DiscoverableState,
    suppress_connections: SuppressConnectionsState,
    config_settings: build_config::Config,
    peers: HashMap<PeerId, Inspectable<Peer>>,

    // Sender end of a futures::mpsc channel to send LocalServiceRequests to Generic Access Service.
    // When a new host adapter is recognized, we create a new GasProxy, which takes GAS requests
    // from the new host and forwards them along a clone of this channel to GAS
    gas_channel_sender: mpsc::Sender<LocalServiceRequest>,

    pairing_dispatcher: Option<PairingDispatcherHandle>,

    watch_peers_publisher: hanging_get::Publisher<HashMap<PeerId, Peer>>,
    watch_peers_registrar: hanging_get::SubscriptionRegistrar<PeerWatcher>,

    watch_hosts_publisher: hanging_get::Publisher<Vec<HostInfo>>,
    watch_hosts_registrar: hanging_get::SubscriptionRegistrar<sys::HostWatcherWatchResponder>,

    // Pending requests to obtain a Host.
    host_requests: Slab<Waker>,

    inspect: HostDispatcherInspect,
}

impl HostDispatcherState {
    /// Set the active adapter for this HostDispatcher
    pub fn set_active_host(&mut self, adapter_id: HostId) -> types::Result<()> {
        if !self.host_devices.contains_key(&adapter_id) {
            return Err(types::Error::no_host());
        }

        if let Some(id) = self.active_id {
            if id == adapter_id {
                return Ok(());
            }

            // Shut down the previously active host.
            let _ = self.host_devices[&id].shutdown();
        }

        self.set_active_id(Some(adapter_id));
        Ok(())
    }

    /// Used to set the pairing delegate. If there is a prior pairing delegate connected to the
    /// host, check if the existing stored connection is closed:
    ///  * if it is closed, overwrite it and succeed
    ///  * if it is still active, fail
    /// If there is no prior delegate, this will always succeed
    /// Returns `true` if the delegate was set successfully, otherwise false
    fn set_pairing_delegate(
        &mut self,
        delegate: PairingDelegateProxy,
        input: InputCapability,
        output: OutputCapability,
    ) -> types::Result<()> {
        match self.pairing_dispatcher.as_ref() {
            Some(dispatcher) if !dispatcher.is_closed() => {
                Err(format_err!("Another Delegate is active"))?
            }
            _ => {
                let (mut dispatcher, handle) = PairingDispatcher::new(delegate, input, output);
                let _ = dispatcher.iattach(&self.inspect._inspect, "pairing_delegate");
                for host in self.host_devices.values() {
                    handle.add_host(host.id(), host.proxy().clone());
                }
                // Old pairing dispatcher dropped; this drops all host pairings
                self.pairing_dispatcher = Some(handle);
                // Spawn handling of the new pairing requests
                // TODO(https://fxbug.dev/42152480) - We should avoid detach() here, and consider a more
                // explicit way to track this task
                fasync::Task::spawn(dispatcher.run()).detach();
                Ok(())
            }
        }
    }

    /// Return the active id. If the ID is currently not set,
    /// it will make the first ID in it's host_devices active
    fn get_active_id(&mut self) -> Option<HostId> {
        let active = self.active_id.clone();
        active.or_else(|| {
            self.host_devices.keys().next().cloned().map(|id| {
                self.set_active_id(Some(id));
                id
            })
        })
    }

    /// Return the active host. If the Host is currently not set,
    /// it will make the first ID in it's host_devices active
    fn get_active_host(&mut self) -> Option<HostDevice> {
        self.get_active_id().and_then(|id| self.host_devices.get(&id)).cloned()
    }

    /// Resolves all pending OnAdapterFuture's. Called when we leave the init period (by seeing the
    /// first host device or when the init timer expires).
    fn resolve_host_requests(&mut self) {
        for waker in &self.host_requests {
            waker.1.wake_by_ref();
        }
    }

    /// Reserves `id` and `address` for a host currently being initialized, rejecting
    /// duplicate `HostId`s or colliding `Address`es before any sensitive identity/bonding data
    /// is sent to the host.
    fn try_reserve_host(&mut self, id: HostId, address: Address) -> Result<(), Error> {
        if self.host_devices.contains_key(&id) || self.initializing_hosts.contains_key(&id) {
            return Err(format_err!("Host with id {id} is already registered or initializing"));
        }
        if self.host_devices.values().any(|h| h.public_address() == address)
            || self.initializing_hosts.values().any(|&a| a == address)
        {
            return Err(format_err!(
                "Host address {address} collides with an already-registered or initializing host"
            ));
        }
        let _ = self.initializing_hosts.insert(id, address);
        Ok(())
    }

    fn add_host(&mut self, id: HostId, host: HostDevice) {
        let _ = self.initializing_hosts.remove(&id);
        if self.host_devices.insert(id, host).is_some() {
            warn!("Host replaced: {}", id)
        } else {
            info!("Host added: {}", id);
        }

        // If this is the only host, mark it as active.
        let _ = self.get_active_id();

        // Update inspect state
        self.inspect.host_count.set(self.host_devices.len() as u64);

        // Notify HostWatcher interface clients about the new device.
        self.notify_host_watchers();

        // Resolve pending adapter futures.
        self.resolve_host_requests();
    }

    /// Updates the active adapter and notifies listeners & host watchers.
    fn set_active_id(&mut self, id: Option<HostId>) {
        info!(
            "New active adapter: {}",
            id.map_or_else(|| "<none>".to_string(), |id| id.to_string())
        );
        self.active_id = id;
        self.notify_host_watchers();
    }

    pub fn notify_host_watchers(&self) {
        // The HostInfo::active field for the active host must be filled in later.
        let active_id = self.active_id;

        // Wait for the hanging get watcher to update so we can linearize updates
        let current_hosts: Vec<HostInfo> = self
            .host_devices
            .values()
            .map(|host| {
                let mut info = host.info();
                // Fill in HostInfo::active
                if let Some(active_id) = active_id {
                    info.active = active_id == host.id();
                }
                info
            })
            .collect();
        let mut publisher = self.watch_hosts_publisher.clone();
        fasync::Task::spawn(async move {
            publisher
                .set(current_hosts)
                .await
                .expect("Fatal error: Host Watcher HangingGet unreachable");
        })
        .detach();
    }
}

#[derive(Clone)]
pub struct HostDispatcher {
    state: Arc<RwLock<HostDispatcherState>>,
}

impl HostDispatcher {
    /// The HostDispatcher will forward all Generic Access Service requests to the mpsc::Receiver
    /// end of |gas_channel_sender|. It is the responsibility of this function's caller to ensure
    /// that these requests are handled. This can be done by passing the mpsc::Receiver into a
    /// GenericAccessService struct and ensuring its run method is scheduled.
    pub fn new(
        appearance: Appearance,
        config: build_config::Config,
        stash: Stash,
        inspect: inspect::Node,
        gas_channel_sender: mpsc::Sender<LocalServiceRequest>,
        watch_peers_publisher: hanging_get::Publisher<HashMap<PeerId, Peer>>,
        watch_peers_registrar: hanging_get::SubscriptionRegistrar<PeerWatcher>,
        watch_hosts_publisher: hanging_get::Publisher<Vec<HostInfo>>,
        watch_hosts_registrar: hanging_get::SubscriptionRegistrar<sys::HostWatcherWatchResponder>,
    ) -> HostDispatcher {
        let hd = HostDispatcherState {
            active_id: None,
            host_devices: HashMap::new(),
            initializing_hosts: HashMap::new(),
            name: None,
            appearance,
            config_settings: config,
            peers: HashMap::new(),
            gas_channel_sender,
            stash,
            discovery: DiscoveryState::NotDiscovering,
            discoverable: DiscoverableState::NotDiscoverable,
            suppress_connections: SuppressConnectionsState::NotSuppressed,
            pairing_dispatcher: None,
            watch_peers_publisher,
            watch_peers_registrar,
            watch_hosts_publisher,
            watch_hosts_registrar,
            host_requests: Slab::new(),
            inspect: HostDispatcherInspect::new(inspect),
        };
        HostDispatcher { state: Arc::new(RwLock::new(hd)) }
    }

    pub fn when_hosts_found(&self) -> impl Future<Output = HostDispatcher> {
        WhenHostsFound::new(self.clone())
    }

    pub fn get_name(&self) -> String {
        self.state.read().name.clone().unwrap_or_else(|| DEFAULT_DEVICE_NAME.to_string())
    }

    pub fn get_appearance(&self) -> Appearance {
        self.state.read().appearance
    }

    pub async fn set_name(&self, name: String, replace: NameReplace) -> types::Result<()> {
        if NameReplace::Keep == replace && self.state.read().name.is_some() {
            return Ok(());
        }
        self.state.write().name = Some(name);
        match self.active_host().await {
            Some(host) => {
                let name = self.get_name();
                host.set_name(name).await
            }
            None => Err(types::Error::no_host()),
        }
    }

    pub async fn set_device_class(&self, class: DeviceClass) -> types::Result<()> {
        let class_repr = class.debug();
        let res = match self.active_host().await {
            Some(host) => host.set_device_class(class).await,
            None => Err(types::Error::no_host()),
        };

        // Update Inspect state
        if res.is_ok() {
            self.state.read().inspect.device_class.set(&class_repr);
        }
        res
    }

    /// Set the active adapter for this HostDispatcher
    pub fn set_active_host(&self, host: HostId) -> types::Result<()> {
        self.state.write().set_active_host(host)
    }

    /// Used to set the pairing delegate. If there is a prior pairing delegate connected to the
    /// host, check if the existing stored connection is closed:
    ///  * if it is closed, overwrite it and succeed
    ///  * if it is still active, fail
    /// If there is no prior delegate, this will always succeed
    /// Returns `true` if the delegate was set successfully, otherwise false
    pub fn set_pairing_delegate(
        &self,
        delegate: PairingDelegateProxy,
        input: InputCapability,
        output: OutputCapability,
    ) -> types::Result<()> {
        self.state.write().set_pairing_delegate(delegate, input, output)
    }

    pub async fn apply_sys_settings(&self, new_settings: sys::Settings) -> build_config::Config {
        let (host_devices, new_config) = {
            let mut state = self.state.write();
            state.config_settings = state.config_settings.update_with_sys_settings(&new_settings);
            (state.host_devices.clone(), state.config_settings.clone())
        };
        for (host_id, device) in host_devices {
            let fut = device.apply_sys_settings(&new_settings);
            if let Err(e) = fut.await {
                warn!("Unable to apply new settings to host {}: {:?}", host_id, e);
                let failed_host_path = device.path();
                self.rm_device(failed_host_path).await;
            }
        }
        new_config
    }

    async fn discover_on_active_host(&self) -> types::Result<DiscoverySessionProxy> {
        match self.active_host().await {
            Some(host) => HostDevice::start_discovery(&host),
            None => Err(types::Error::no_host()),
        }
    }

    fn make_discovery_on_closed_task(&self, proxy: DiscoverySessionProxy) -> fasync::Task<()> {
        fasync::Task::spawn(self.clone().process_discovery_on_closed(proxy))
    }

    async fn process_discovery_on_closed(self, proxy: DiscoverySessionProxy) {
        // wait for DiscoverySession to close
        let _ = proxy.on_closed().await;
        debug!(
            "process_discovery_event_stream: Discovery protocol closed (state: {:?})",
            self.state.read().discovery
        );

        let old_state =
            std::mem::replace(&mut self.state.write().discovery, DiscoveryState::NotDiscovering);
        match old_state {
            DiscoveryState::NotDiscovering | DiscoveryState::Pending { .. } => {
                warn!(
                    "process_discovery_event_stream: Unexpected discovery event stream close in state {:?}",
                    old_state
                );
            }
            DiscoveryState::Discovering { discovery_stopped_sender, .. } => {
                let _ = discovery_stopped_sender.send(());
            }
            DiscoveryState::Stopping { discovery_stopped_sender, session_sender, .. } => {
                let _ = discovery_stopped_sender.send(());

                // Restart discovery if clients queued session watchers while stopping.
                if let Some(sender) = session_sender {
                    // On errors all session_watchers will be dropped, signaling receivers of
                    // the error.
                    if let Ok(session) = self.start_discovery().await {
                        let _ = sender.send(session);
                    }
                }
            }
        };
        trace!("discovery_event_stream_task: completed");
    }

    fn make_start_discovery_task(
        &self,
        session: Arc<DiscoverySession>,
        started: fasync::MonotonicInstant,
    ) -> fasync::Task<()> {
        let hd = self.clone();
        fasync::Task::spawn(async move {
            debug!("start_discovery_task: waiting for discover_on_active_host");
            let Ok(discovery_proxy) = hd.discover_on_active_host().await else {
                // On failure (host init timeout), revert state to NotDiscovering and drop Pending
                // state to notify Receivers.
                hd.state.write().discovery = DiscoveryState::NotDiscovering;
                return;
            };
            debug!("start_discovery_task: started discovery successfully");
            let _ = hd.state.read().inspect.discovery_sessions.add(1);

            let discovery_on_closed_task =
                hd.make_discovery_on_closed_task(discovery_proxy.clone());

            // Replace Pending state with new session and send session token to waiters
            let old_state =
                std::mem::replace(&mut hd.state.write().discovery, DiscoveryState::NotDiscovering);
            if let DiscoveryState::Pending {
                session_receiver: _,
                session_sender,
                discovery_stopped_receiver,
                discovery_stopped_sender,
                start_discovery_task: _,
            } = old_state
            {
                hd.state.write().discovery = DiscoveryState::Discovering {
                    session: Arc::downgrade(&session),
                    discovery_proxy,
                    started,
                    discovery_on_closed_task,
                    discovery_stopped_receiver,
                    discovery_stopped_sender,
                };
                let _ = session_sender.send(session.clone());
            }
            trace!("start_discovery_task: completed");
        })
    }

    pub async fn start_discovery(&self) -> types::Result<Arc<DiscoverySession>> {
        let session_fut = self.state.write().discovery.get_active_session_future();
        if let Some(session_fut) = session_fut {
            debug!(
                "start_discovery: awaiting DiscoverySession (state: {:?})",
                self.state.read().discovery
            );
            return session_fut
                .await
                .map_err(|_| format_err!("Pending discovery client channel closed").into());
        }

        let session = Arc::new(DiscoverySession { dispatcher_state: self.state.clone() });
        let started = fasync::MonotonicInstant::now();
        let (session_sender, session_receiver) = oneshot::channel();
        let session_receiver = session_receiver.shared();
        let (discovery_stopped_sender, discovery_stopped_receiver) = oneshot::channel();
        let discovery_stopped_receiver = discovery_stopped_receiver.shared();
        let start_discovery_task = self.make_start_discovery_task(session, started);

        // Immediately mark the state as pending to indicate to other requests to wait on
        // this discovery session initialization
        self.state.write().discovery = DiscoveryState::Pending {
            session_receiver: session_receiver.clone(),
            session_sender,
            discovery_stopped_receiver: discovery_stopped_receiver.clone(),
            discovery_stopped_sender,
            start_discovery_task,
        };

        debug!(
            "start_discovery: awaiting DiscoverySession for first client (state: {:?})",
            self.state.read().discovery
        );
        session_receiver
            .await
            .map_err(|_| format_err!("Pending discovery client channel closed").into())
    }

    fn make_set_discoverable_task(&self, session: Arc<DiscoverableSession>) -> fasync::Task<()> {
        let hd = self.clone();
        fasync::Task::spawn(async move {
            debug!("set_discoverable_task: waiting for set_discoverable_on_active_host");
            let Ok(host) = hd.set_discoverable_on_active_host().await else {
                // On failure (host init timeout), revert state to NotDiscoverable if still in the
                // Pending state for this session, and drop Pending state outside the write lock to
                // notify Receivers.
                let _old_state = hd.state.write().discoverable.clear_pending_if_matching(&session);
                return;
            };
            debug!("set_discoverable_task: established discoverable session on host successfully");

            if let Some(sender) =
                hd.state.write().discoverable.set_discoverable(host.id(), &session)
            {
                let _ = sender.send(session);
            }
            trace!("set_discoverable_task: completed");
        })
    }

    async fn set_discoverable_on_active_host(&self) -> types::Result<HostDevice> {
        match self.active_host().await {
            Some(host) => {
                host.set_discoverable(true).await?;
                Ok(host)
            }
            None => Err(types::Error::no_host()),
        }
    }

    pub(crate) async fn set_discoverable(&self) -> types::Result<Arc<DiscoverableSession>> {
        let session_fut = self.state.read().discoverable.active_session_future();
        if let Some(session_fut) = session_fut {
            debug!(
                "set_discoverable: awaiting DiscoverableSession (state: {:?})",
                self.state.read().discoverable
            );
            return session_fut
                .await
                .map_err(|_| format_err!("Pending discoverable client channel closed").into());
        }

        let session = Arc::new(DiscoverableSession { dispatcher_state: self.state.clone() });
        let weak_session = Arc::downgrade(&session);
        let (session_sender, session_receiver) = oneshot::channel();
        let session_receiver = session_receiver.shared();
        let start_discoverable_task = self.make_set_discoverable_task(session);

        // Immediately mark the state as pending to indicate to other requests to wait on
        // this discoverable session initialization
        self.state.write().discoverable = DiscoverableState::Pending {
            session: weak_session,
            session_receiver: session_receiver.clone(),
            session_sender,
            _start_discoverable_task: start_discoverable_task,
        };

        debug!(
            "set_discoverable: awaiting DiscoverableSession for first client (state: {:?})",
            self.state.read().discoverable
        );
        session_receiver
            .await
            .map_err(|_| format_err!("Pending discoverable client channel closed").into())
    }

    fn make_suppress_connections_task(
        &self,
        session: Arc<SuppressConnectionsSession>,
    ) -> fasync::Task<()> {
        let hd = self.clone();
        fasync::Task::spawn(async move {
            debug!("suppress_connections_task: waiting for suppress_connections_on_active_host");
            let Ok(host) = hd.suppress_connections_on_active_host().await else {
                // On failure (host init timeout), revert state to NotSuppressed and drop Pending
                // state to notify Receivers.
                hd.state.write().suppress_connections = SuppressConnectionsState::NotSuppressed;
                return;
            };
            debug!("suppress_connections_task: suppressed connections on host successfully");

            if let Some(sender) =
                hd.state.write().suppress_connections.set_suppressed(host.id(), &session)
            {
                let _ = sender.send(session);
            }
            trace!("suppress_connections_task: completed");
        })
    }

    async fn suppress_connections_on_active_host(&self) -> types::Result<HostDevice> {
        match self.active_host().await {
            Some(host) => {
                host.set_connectable(false).await?;
                Ok(host)
            }
            None => Err(types::Error::no_host()),
        }
    }

    pub async fn suppress_connections(&self) -> types::Result<Arc<SuppressConnectionsSession>> {
        let session_fut = self.state.read().suppress_connections.get_active_session_future();
        if let Some(session_fut) = session_fut {
            debug!(
                "suppress_connections: awaiting SuppressConnectionsSession (state: {:?})",
                self.state.read().suppress_connections
            );
            return session_fut.await.map_err(|_| {
                format_err!("Pending connection suppression client channel closed").into()
            });
        }

        let session = Arc::new(SuppressConnectionsSession { dispatcher_state: self.state.clone() });
        let (session_sender, session_receiver) = oneshot::channel();
        let session_receiver = session_receiver.shared();
        let start_suppression_task = self.make_suppress_connections_task(session);

        // Immediately mark the state as pending to indicate to other requests to wait on
        // this suppression session initialization
        self.state.write().suppress_connections = SuppressConnectionsState::Pending {
            session_receiver: session_receiver.clone(),
            session_sender,
            _start_suppression_task: start_suppression_task,
        };

        debug!(
            "suppress_connections: awaiting SuppressConnectionsSession for first client (state: {:?})",
            self.state.read().suppress_connections
        );
        session_receiver
            .await
            .map_err(|_| format_err!("Pending connection suppression client channel closed").into())
    }

    pub(crate) fn stash(&self) -> Stash {
        self.state.read().stash.clone()
    }

    pub async fn forget(&self, peer_id: PeerId) -> types::Result<()> {
        // Try to delete from each host, even if it might not have the peer.
        // peers will be updated by the disconnection(s).
        let hosts = self.get_all_adapters().await;
        if hosts.is_empty() {
            return Err(sys::Error::Failed.into());
        }
        let mut hosts_removed: u32 = 0;
        for host in hosts {
            let host_path = host.path();

            match host.forget(peer_id).await {
                Ok(()) => hosts_removed += 1,
                Err(types::Error::SysError(sys::Error::PeerNotFound)) => {
                    trace!("No peer {} on host {:?}; ignoring", peer_id, host_path);
                }
                err => {
                    error!("Could not forget peer {} on host {:?}", peer_id, host_path);
                    return err;
                }
            }
        }

        if let Err(_) = self.stash().rm_peer(peer_id).await {
            return Err(format_err!("Couldn't remove peer").into());
        }

        if hosts_removed == 0 {
            return Err(format_err!("No hosts had peer").into());
        }
        Ok(())
    }

    pub async fn connect(&self, peer_id: PeerId) -> types::Result<()> {
        let host = self.active_host().await;
        match host {
            Some(host) => host.connect(peer_id).await,
            None => Err(types::Error::SysError(sys::Error::Failed)),
        }
    }

    /// Instruct the active host to intitiate a pairing procedure with the target peer. If it
    /// fails, we return the error we receive from the host
    pub async fn pair(&self, id: PeerId, pairing_options: PairingOptions) -> types::Result<()> {
        let host = self.active_host().await;
        match host {
            Some(host) => host.pair(id, pairing_options.into()).await,
            None => Err(sys::Error::Failed.into()),
        }
    }

    // Attempt to disconnect peer with id `peer_id` from all transports
    pub async fn disconnect(&self, peer_id: PeerId) -> types::Result<()> {
        let host = self.active_host().await;
        match host {
            Some(host) => host.disconnect(peer_id).await,
            None => Err(types::Error::no_host()),
        }
    }

    pub fn active_host(&self) -> impl Future<Output = Option<HostDevice>> {
        self.when_hosts_found().map(|adapter| {
            let mut wstate = adapter.state.write();
            wstate.get_active_host()
        })
    }

    pub async fn get_all_adapters(&self) -> Vec<HostDevice> {
        let _ = self.when_hosts_found().await;
        self.state.read().host_devices.values().cloned().collect()
    }

    #[cfg(test)]
    pub fn get_adapters(&self) -> Vec<HostInfo> {
        let hosts = self.state.read();
        hosts.host_devices.values().map(|host| host.info()).collect()
    }

    pub async fn request_host_service(self, chan: zx::Channel, service: HostService) {
        match self.active_host().await {
            Some(host) => {
                let host = host.proxy();
                match service {
                    HostService::LeCentral => {
                        let remote = ServerEnd::<CentralMarker>::new(chan.into());
                        let _ = host.request_protocol(ProtocolRequest::Central(remote));
                    }
                    HostService::LePeripheral => {
                        let remote = ServerEnd::<PeripheralMarker>::new(chan.into());
                        let _ = host.request_protocol(ProtocolRequest::Peripheral(remote));
                    }
                    HostService::LePrivilegedPeripheral => {
                        let remote = ServerEnd::<PrivilegedPeripheralMarker>::new(chan.into());
                        let _ =
                            host.request_protocol(ProtocolRequest::PrivilegedPeripheral(remote));
                    }
                    HostService::LeGatt => {
                        let remote = ServerEnd::<Server_Marker>::new(chan.into());
                        let _ = host.request_protocol(ProtocolRequest::GattServer(remote));
                    }
                    HostService::LeGatt2 => {
                        let remote = ServerEnd::<Server_Marker2>::new(chan.into());
                        let _ = host.request_protocol(ProtocolRequest::Gatt2Server(remote));
                    }
                    HostService::Profile => {
                        let remote = ServerEnd::<ProfileMarker>::new(chan.into());
                        let _ = host.request_protocol(ProtocolRequest::Profile(remote));
                    }
                }
            }
            None => eprintln!("Failed to spawn, no active host"),
        }
    }

    // This is not an async method as we do not want to borrow `self` for the duration of the async
    // call, and we also want to trigger the send immediately even if the future is not yet awaited
    pub fn store_bond(
        &self,
        bond_data: BondingData,
    ) -> impl Future<Output = Result<(), Error>> + use<> {
        self.stash().store_bond(bond_data)
    }

    pub fn on_device_updated(&self, peer: Peer) -> impl Future<Output = ()> + use<> {
        let update_peer = peer.clone();

        let mut publisher = {
            let mut state = self.state.write();

            let node = state.inspect.peers().create_child(unique_name("peer_"));
            let peer = Inspectable::new(peer, node);
            let _drop_old_value = state.peers.insert(peer.id.clone(), peer);
            state.inspect.peer_count.set(state.peers.len() as u64);
            state.watch_peers_publisher.clone()
        };

        // Wait for the hanging get watcher to update so we can linearize updates
        async move {
            publisher
                .update(move |peers| {
                    let _ = peers.insert(update_peer.id, update_peer);
                    true
                })
                .await
                .expect("Fatal error: Peer Watcher HangingGet unreachable")
        }
    }

    pub fn on_device_removed(&self, id: PeerId) -> impl Future<Output = ()> + use<> {
        let mut publisher = {
            let mut state = self.state.write();
            if let Some(removed) = state.peers.remove(&id) {
                inspect_log!(state.inspect.evicted_peers, peer: removed);
            }
            state.inspect.peer_count.set(state.peers.len() as u64);
            state.watch_peers_publisher.clone()
        };

        // Wait for the hanging get watcher to update so we can linearize updates
        async move {
            publisher
                .update(move |peers| {
                    // Updated if we actually removed something.
                    peers.remove(&id).is_some()
                })
                .await
                .expect("Fatal error: Peer Watcher HangingGet unreachable")
        }
    }

    pub async fn watch_peers(&self) -> hanging_get::Subscriber<PeerWatcher> {
        let mut registrar = self.state.write().watch_peers_registrar.clone();
        registrar.new_subscriber().await.expect("Fatal error: Peer Watcher HangingGet unreachable")
    }

    pub async fn watch_hosts(&self) -> hanging_get::Subscriber<sys::HostWatcherWatchResponder> {
        let mut registrar = self.state.write().watch_hosts_registrar.clone();
        registrar.new_subscriber().await.expect("Fatal error: Host Watcher HangingGet unreachable")
    }

    async fn spawn_gas_proxy(&self, gatt_server_proxy: Server_Proxy) -> Result<(), Error> {
        let gas_channel = self.state.read().gas_channel_sender.clone();
        let gas_proxy =
            generic_access_service::GasProxy::new(gatt_server_proxy, gas_channel).await?;
        fasync::Task::spawn(gas_proxy.run().map(|r| {
            r.unwrap_or_else(|err| {
                warn!("Error passing message through Generic Access proxy: {:?}", err);
            })
        }))
        .detach();
        Ok(())
    }

    /// Commit all bootstrapped bonding identities to the system. This will update both the Stash
    /// and our in memory store, and notify all hosts of new bonding identities. If we already have
    /// bonding data for any of the peers (as identified by address), the new bootstrapped data
    /// will override them.
    pub async fn commit_bootstrap(&self, identities: Vec<Identity>) -> types::Result<()> {
        // Store all new bonds in our permanent Store. If we cannot successfully record the bonds
        // in the store, then Bootstrap.Commit() has failed.
        let mut stash = self.state.read().stash.clone();
        for identity in identities {
            stash.store_bonds(identity.bonds).await?
        }

        // Notify all current hosts of any changes to their bonding data
        let host_devices: Vec<_> = self.state.read().host_devices.values().cloned().collect();

        for host in host_devices {
            // If we fail to restore bonds to a given host, that is not a failure on a part of
            // Bootstrap.Commit(), but a failure on the host. So do not return error from this
            // function, but instead log and continue.
            // TODO(https://fxbug.dev/42121837) - if a host fails we should close it and clean up after it
            if let Err(error) =
                try_restore_bonds(host.clone(), self.clone(), &host.public_address()).await
            {
                error!(
                    "Error restoring Bootstrapped bonds to host '{:?}': {}",
                    host.debug_identifiers(),
                    error
                )
            }
        }
        Ok(())
    }

    /// Finishes initializing a host device by setting host configs and services.
    async fn add_host_device(&self, host_device: &HostDevice) -> Result<(), Error> {
        let dbg_ids = host_device.debug_identifiers();
        let id = host_device.id();
        let address = host_device.public_address();

        self.state.write().try_reserve_host(id, address).context(format!(
            "{:?}: rejected duplicate or colliding bt-host registration",
            dbg_ids
        ))?;

        struct ReservationGuard<'a>(&'a RwLock<HostDispatcherState>, HostId);
        impl Drop for ReservationGuard<'_> {
            fn drop(&mut self) {
                let _ = self.0.write().initializing_hosts.remove(&self.1);
            }
        }
        let _reservation_guard = ReservationGuard(&self.state, id);

        // TODO(https://fxbug.dev/42145442): Make sure that the bt-host device is left in a well-known state if
        // any of these operations fails.

        assign_host_data(host_device.clone(), self.clone(), &address)
            .await
            .context(format!("{:?}: failed to assign identity to bt-host", dbg_ids))?;
        if let Err(e) = try_restore_bonds(host_device.clone(), self.clone(), &address).await {
            error!("{:?}: failed to restore persisted bonds to bt-host: {}", dbg_ids, e);
        }

        let config = self.state.read().config_settings.clone();
        host_device
            .apply_config(config)
            .await
            .context(format!("{:?}: failed to configure bt-host device", dbg_ids))?;

        // Assign the name that is currently assigned to the HostDispatcher as the local name.
        let name = self.get_name();
        host_device
            .set_name(name)
            .await
            .map_err(|e| e.as_failure())
            .context(format!("{:?}: failed to set name of bt-host", dbg_ids))?;

        let (gatt_server_proxy, remote_gatt_server) = fidl::endpoints::create_proxy();
        host_device
            .proxy()
            .request_protocol(ProtocolRequest::Gatt2Server(remote_gatt_server))
            .context(format!("{:?}: failed to open gatt server for bt-host", dbg_ids))?;
        self.spawn_gas_proxy(gatt_server_proxy)
            .await
            .context(format!("{:?}: failed to spawn generic access service", dbg_ids))?;

        // Ensure the current active pairing delegate (if it exists) handles this host
        self.handle_pairing_requests(host_device.clone());

        let became_active = {
            let mut state = self.state.write();
            let was_active = state.active_id.is_some();
            state.add_host(id, host_device.clone());
            !was_active && state.active_id.is_some()
        };

        if became_active {
            if let Err(err) = self.configure_newly_active_adapter().await {
                warn!("Failed to persist state on adapter add: {:?}", err);
            }
        }

        Ok(())
    }

    // Update our hanging_get server with the latest hosts. This will notify any pending
    // hanging_gets and any new requests will see the new results.
    fn notify_host_watchers(&self) {
        self.state.write().notify_host_watchers();
    }

    pub async fn rm_device(&self, host_path: &str) {
        let mut new_adapter_activated = false;
        // Scope our HostDispatcherState lock
        {
            let mut hd = self.state.write();
            let active_id = hd.active_id.clone();

            // Get the host IDs that match `host_path`.
            let ids: Vec<HostId> = hd
                .host_devices
                .iter()
                .filter(|(_, host)| host.path() == host_path)
                .map(|(k, _)| k.clone())
                .collect();

            let id_strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            info!("Host removed: {} (path: {:?})", id_strs.join(","), host_path);

            for id in &ids {
                drop(hd.host_devices.remove(id));
            }

            // Reset the active ID if it got removed.
            if let Some(active_id) = active_id {
                if ids.contains(&active_id) {
                    hd.active_id = None;
                }
            }

            // Try to assign a new active adapter. This may send an "OnActiveAdapterChanged" event.
            if hd.active_id.is_none() && hd.get_active_id().is_some() {
                new_adapter_activated = true;
            }
        } // Now the lock is dropped, we can run the async notify

        if new_adapter_activated {
            if let Err(err) = self.configure_newly_active_adapter().await {
                warn!("Failed to persist state on adapter change: {:?}", err);
            }
        }
        self.notify_host_watchers();
    }

    async fn migrate_discovery_to_active_host(&self) {
        let old_state =
            std::mem::replace(&mut self.state.write().discovery, DiscoveryState::NotDiscovering);
        match old_state {
            DiscoveryState::NotDiscovering => {}
            DiscoveryState::Pending {
                session_receiver,
                session_sender,
                discovery_stopped_receiver,
                discovery_stopped_sender,
                start_discovery_task,
            } => {
                info!("migrating Pending discovery to new host");
                // Stop the old discovery task, and restart discovery on the new host.
                drop(start_discovery_task);
                let session = Arc::new(DiscoverySession { dispatcher_state: self.state.clone() });
                let started = fasync::MonotonicInstant::now();
                let start_discovery_task = self.make_start_discovery_task(session, started);
                self.state.write().discovery = DiscoveryState::Pending {
                    session_receiver,
                    session_sender,
                    discovery_stopped_receiver,
                    discovery_stopped_sender,
                    start_discovery_task,
                };
            }
            DiscoveryState::Discovering {
                session,
                discovery_proxy,
                started,
                discovery_on_closed_task,
                discovery_stopped_receiver,
                discovery_stopped_sender,
            } => {
                info!("migrating Discovering discovery to new host");
                drop(discovery_on_closed_task);
                drop(discovery_proxy);

                let Some(session) = session.upgrade() else {
                    warn!(
                        "Failed to upgrade discovery session during migration; all clients dropped token"
                    );
                    return;
                };

                // Restart discovery.
                let (session_sender, session_receiver) = oneshot::channel();
                let session_receiver = session_receiver.shared();
                let start_discovery_task = self.make_start_discovery_task(session, started);
                self.state.write().discovery = DiscoveryState::Pending {
                    session_receiver: session_receiver.clone(),
                    session_sender,
                    discovery_stopped_receiver,
                    discovery_stopped_sender,
                    start_discovery_task,
                };
                if let Err(e) = session_receiver.await {
                    warn!("Pending discovery client channel closed during migration: {e:?}");
                }
            }
            DiscoveryState::Stopping {
                discovery_proxy,
                session_receiver: _,
                session_sender,
                discovery_on_closed_task: discovery_event_stream_task,
                discovery_stopped_receiver: _,
                discovery_stopped_sender,
            } => {
                info!("migrating Stopping discovery to new host");
                drop(discovery_event_stream_task);
                drop(discovery_proxy);
                let _ = discovery_stopped_sender.send(());

                // Restart discovery if clients queued session receiver while stopping.
                if let Some(sender) = session_sender {
                    // On errors, sender will be dropped, signaling receivers of
                    // the error.
                    if let Ok(session) = self.start_discovery().await {
                        let _ = sender.send(session);
                    }
                }
            }
        }
    }

    fn migrate_discoverable_to_active_host(&self) {
        let old_state = std::mem::replace(
            &mut self.state.write().discoverable,
            DiscoverableState::NotDiscoverable,
        );
        match old_state {
            DiscoverableState::NotDiscoverable => {}
            DiscoverableState::Pending {
                session,
                session_receiver,
                session_sender,
                _start_discoverable_task,
            } => {
                info!("migrating Pending discoverable to new host");
                // Stop the old discoverable task, and restart on the new host.
                let session = session.upgrade().unwrap_or_else(|| {
                    Arc::new(DiscoverableSession { dispatcher_state: self.state.clone() })
                });
                drop(_start_discoverable_task);
                let weak_session = Arc::downgrade(&session);
                let start_discoverable_task = self.make_set_discoverable_task(session);
                self.state.write().discoverable = DiscoverableState::Pending {
                    session: weak_session,
                    session_receiver,
                    session_sender,
                    _start_discoverable_task: start_discoverable_task,
                };
            }
            DiscoverableState::Discoverable { session, host_id } => {
                let mut write = self.state.write();
                if write.active_id == Some(host_id) {
                    write.discoverable = DiscoverableState::Discoverable { session, host_id };
                    return;
                }
                info!("migrating Discoverable state to new host");
                let Some(strong_session) = session.upgrade() else {
                    return;
                };

                let (session_sender, session_receiver) = oneshot::channel();
                let session_receiver = session_receiver.shared();
                let start_discoverable_task = self.make_set_discoverable_task(strong_session);
                write.discoverable = DiscoverableState::Pending {
                    session,
                    session_receiver,
                    session_sender,
                    _start_discoverable_task: start_discoverable_task,
                };
            }
        }
    }

    fn migrate_suppress_connections_to_active_host(&self) {
        let mut write = self.state.write();
        let old_state = std::mem::replace(
            &mut write.suppress_connections,
            SuppressConnectionsState::NotSuppressed,
        );
        match old_state {
            SuppressConnectionsState::NotSuppressed => {}
            SuppressConnectionsState::Pending {
                session_receiver,
                session_sender,
                _start_suppression_task,
            } => {
                info!("migrating Pending connection suppression to new host");
                // Stop the old suppression task, and restart suppression on the new host.
                drop(_start_suppression_task);
                let session =
                    Arc::new(SuppressConnectionsSession { dispatcher_state: self.state.clone() });
                let start_suppression_task = self.make_suppress_connections_task(session);
                write.suppress_connections = SuppressConnectionsState::Pending {
                    session_receiver,
                    session_sender,
                    _start_suppression_task: start_suppression_task,
                };
            }
            SuppressConnectionsState::Suppressed { session, host_id } => {
                if write.active_id == Some(host_id) {
                    write.suppress_connections =
                        SuppressConnectionsState::Suppressed { session, host_id };
                    return;
                }
                info!("migrating Suppressed connection suppression to new host");
                let Some(session) = session.upgrade() else {
                    return;
                };

                let (session_sender, session_receiver) = oneshot::channel();
                let session_receiver = session_receiver.shared();
                let start_suppression_task = self.make_suppress_connections_task(session);
                write.suppress_connections = SuppressConnectionsState::Pending {
                    session_receiver,
                    session_sender,
                    _start_suppression_task: start_suppression_task,
                };
            }
        }
    }

    /// Configure a newly active adapter with the correct behavior for an active adapter.
    async fn configure_newly_active_adapter(&self) -> types::Result<()> {
        self.migrate_discovery_to_active_host().await;
        self.migrate_discoverable_to_active_host();
        self.migrate_suppress_connections_to_active_host();
        Ok(())
    }

    /// Route pairing requests from this host through our pairing dispatcher, if it exists
    fn handle_pairing_requests(&self, host: HostDevice) {
        let mut dispatcher = self.state.write();

        if let Some(handle) = &mut dispatcher.pairing_dispatcher {
            handle.add_host(host.id(), host.proxy().clone());
        }
    }

    pub async fn add_host_component(&self, proxy: HostProxy) -> types::Result<()> {
        info!("Adding host component");

        let node = self.state.read().inspect.hosts().create_child(unique_name("device_"));

        let proxy_handle = proxy.as_channel().as_handle_ref().raw_handle().to_string();
        let host_device = init_host(proxy_handle.as_str(), node, proxy).await?;
        info!("Successfully started host device: {:?}", host_device.info());
        self.add_host_device(&host_device).await?;

        // Start listening to Host interface events.
        fasync::Task::spawn({
            let this = self.clone();
            async move {
                match host_device.watch_events(this.clone()).await {
                    Ok(()) => (),
                    Err(e) => {
                        warn!("Error handling host event: {e:?}");
                        let host_path = proxy_handle.as_str();
                        this.rm_device(&host_path).await;
                    }
                }
            }
        })
        .detach();

        Ok(())
    }

    pub fn get_peer_address(&self, peer_id: PeerId) -> Option<Address> {
        self.state.read().peers.get(&peer_id).map(|p| p.address)
    }
}

async fn init_host(path: &str, node: inspect::Node, proxy: HostProxy) -> Result<HostDevice, Error> {
    node.record_string("path", path);

    // Obtain basic information and create and entry in the dispatcher's map.
    let host_info = proxy.watch_state().await.context("failed to obtain bt-host information")?;
    let host_info = Inspectable::new(HostInfo::try_from(host_info)?, node);

    Ok(HostDevice::new(path.to_string(), proxy, host_info))
}

impl HostListener for HostDispatcher {
    type PeerUpdatedFut = BoxFuture<'static, ()>;
    fn on_peer_updated(&mut self, peer: Peer) -> Self::PeerUpdatedFut {
        self.on_device_updated(peer).boxed()
    }
    type PeerRemovedFut = BoxFuture<'static, ()>;
    fn on_peer_removed(&mut self, id: PeerId) -> Self::PeerRemovedFut {
        self.on_device_removed(id).boxed()
    }
    type HostBondFut = BoxFuture<'static, Result<(), anyhow::Error>>;
    fn on_new_host_bond(&mut self, data: BondingData) -> Self::HostBondFut {
        self.store_bond(data).boxed()
    }

    type HostInfoFut = BoxFuture<'static, Result<(), anyhow::Error>>;
    fn on_host_updated(&mut self, _info: HostInfo) -> Self::HostInfoFut {
        self.notify_host_watchers();
        async { Ok(()) }.boxed()
    }
}

/// A future that completes when at least one adapter is available.
#[must_use = "futures do nothing unless polled"]
struct WhenHostsFound {
    hd: HostDispatcher,
    waker_key: Option<usize>,
}

impl WhenHostsFound {
    // Constructs an WhenHostsFound that completes at the latest after HOST_INIT_TIMEOUT
    fn new(hd: HostDispatcher) -> impl Future<Output = HostDispatcher> {
        WhenHostsFound { hd: hd.clone(), waker_key: None }.on_timeout(
            HOST_INIT_TIMEOUT.after_now(),
            move || {
                {
                    let mut inner = hd.state.write();
                    if inner.host_devices.len() == 0 {
                        info!("No bt-host devices found");
                        inner.resolve_host_requests();
                    }
                }
                hd
            },
        )
    }

    fn remove_waker(&mut self) {
        if let Some(key) = self.waker_key {
            drop(self.hd.state.write().host_requests.remove(key));
        }
        self.waker_key = None;
    }
}

impl Drop for WhenHostsFound {
    fn drop(&mut self) {
        self.remove_waker()
    }
}

impl Unpin for WhenHostsFound {}

impl Future for WhenHostsFound {
    type Output = HostDispatcher;

    fn poll(mut self: ::std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.hd.state.read().host_devices.len() == 0 {
            let hd = self.hd.clone();
            if self.waker_key.is_none() {
                self.waker_key = Some(hd.state.write().host_requests.insert(cx.waker().clone()));
            }
            Poll::Pending
        } else {
            self.remove_waker();
            Poll::Ready(self.hd.clone())
        }
    }
}

pub(crate) async fn try_restore_bonds(
    host_device: HostDevice,
    hd: HostDispatcher,
    address: &Address,
) -> types::Result<()> {
    // Load bonding data that use this host's `address` as their "local identity address".
    let opt_data = hd.stash().list_bonds(address.clone()).await?;
    let data = match opt_data {
        Some(data) => data,
        None => return Ok(()),
    };
    match host_device.restore_bonds(data).await {
        Err(e) => {
            error!("failed to restore bonding data for host: {:?}", e);
            Err(e)
        }
        Ok(errors) => {
            if errors.is_empty() {
                Ok(())
            } else {
                // Do not Debug-format `sys::BondingData`: doing so recursively formats
                // secret LTK, IRK, CSRK, and BR/EDR link-key bytes into logs.
                let peers: Vec<Option<PeerId>> =
                    errors.iter().map(|b| b.identifier.map(PeerId::from)).collect();
                let msg = format!("failed to restore bonding data for peers: {:?}", peers);
                error!("{}", msg);
                Err(anyhow!(msg).into())
            }
        }
    }
}

fn generate_irk() -> Result<sys::Key, zx::Status> {
    let mut buf: [u8; 16] = [0; 16];
    // Generate a secure IRK.
    zx::cprng_draw(&mut buf);
    Ok(sys::Key { value: buf })
}

async fn assign_host_data(
    host: HostDevice,
    hd: HostDispatcher,
    address: &Address,
) -> Result<(), Error> {
    // Obtain an existing IRK or generate a new one if one doesn't already exists for |address|.
    let data = match hd.stash().get_host_data(address.clone()).await? {
        Some(host_data) => {
            trace!("restored IRK");
            host_data
        }
        None => {
            // Generate a new IRK.
            trace!("generating new IRK");
            let new_data = HostData { irk: Some(generate_irk()?) };

            if let Err(e) = hd.stash().store_host_data(address.clone(), new_data.clone()).await {
                error!("failed to persist local IRK");
                return Err(e.into());
            }
            new_data
        }
    };
    host.set_local_data(data).map_err(|e| e.into())
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;

    use fidl_fuchsia_bluetooth_gatt2::{
        LocalServiceProxy, Server_Request, Server_RequestStream as GattServerRequestStream,
    };
    use fidl_fuchsia_bluetooth_host::{
        BondingDelegateRequest, BondingDelegateRequestStream, HostRequest, HostRequestStream,
    };
    use futures::StreamExt;
    use futures::future::join;
    use std::collections::HashSet;

    pub(crate) enum RestoreBondsBehavior {
        AcceptAll,
        RejectPeers(HashSet<PeerId>),
    }

    pub(crate) fn make_test_dispatcher(
        watch_peers_publisher: hanging_get::Publisher<HashMap<PeerId, Peer>>,
        watch_peers_registrar: hanging_get::SubscriptionRegistrar<PeerWatcher>,
        watch_hosts_publisher: hanging_get::Publisher<Vec<HostInfo>>,
        watch_hosts_registrar: hanging_get::SubscriptionRegistrar<sys::HostWatcherWatchResponder>,
    ) -> HostDispatcher {
        let (gas_channel_sender, _ignored_gas_task_req_stream) = mpsc::channel(0);
        HostDispatcher::new(
            Appearance::Display,
            Default::default(),
            Stash::in_memory_mock(),
            fuchsia_inspect::Node::default(),
            gas_channel_sender,
            watch_peers_publisher,
            watch_peers_registrar,
            watch_hosts_publisher,
            watch_hosts_registrar,
        )
    }

    pub(crate) fn make_simple_test_dispatcher() -> HostDispatcher {
        let watch_peers_broker = hanging_get::HangingGetBroker::new(
            HashMap::new(),
            |_, _| true,
            hanging_get::DEFAULT_CHANNEL_SIZE,
        );
        let watch_hosts_broker = hanging_get::HangingGetBroker::new(
            Vec::new(),
            |_, _| true,
            hanging_get::DEFAULT_CHANNEL_SIZE,
        );

        let dispatcher = make_test_dispatcher(
            watch_peers_broker.new_publisher(),
            watch_peers_broker.new_registrar(),
            watch_hosts_broker.new_publisher(),
            watch_hosts_broker.new_registrar(),
        );

        let watchers_fut = join(watch_peers_broker.run(), watch_hosts_broker.run()).map(|_| ());
        fasync::Task::spawn(watchers_fut).detach();
        dispatcher
    }

    #[derive(Default)]
    pub(crate) struct GasEndpoints {
        gatt_server: Option<GattServerRequestStream>,
        service: Option<LocalServiceProxy>,
    }

    async fn handle_host_server_init(
        mut host_server: HostRequestStream,
        restore_bonds_behavior: RestoreBondsBehavior,
    ) -> (HostRequestStream, GasEndpoints, BondingDelegateRequestStream) {
        let mut gas_endpoints = GasEndpoints::default();
        let mut bonding_delegate: Option<BondingDelegateRequestStream> = None;
        while gas_endpoints.gatt_server.is_none() || bonding_delegate.is_none() {
            let mut restore_bonds_fut = bonding_delegate
                .as_mut()
                .map(|stream| stream.next().fuse())
                .unwrap_or_else(futures::future::Fuse::terminated);
            futures::select! {
                host_req = host_server.next() => match host_req {
                    Some(Ok(HostRequest::SetLocalData { .. })) => {
                        info!("Setting Local Data");
                    }
                    Some(Ok(HostRequest::SetLocalName { responder, .. })) => {
                        info!("Setting Local Name");
                        let _ = responder.send(Ok(()));
                    }
                    Some(Ok(HostRequest::SetDeviceClass { responder, .. })) => {
                        info!("Setting Device Class");
                        let _ = responder.send(Ok(()));
                    }
                    Some(Ok(HostRequest::RequestProtocol {
                        payload: ProtocolRequest::Gatt2Server(server),
                        ..
                    })) => {
                        // don't respond at all on the server side.
                        info!("Storing Gatt Server");
                        let mut gatt_server = server.into_stream();
                        info!("GAS Server was started, waiting for publish");
                        // The Generic Access Service now publishes itself.
                        match gatt_server.next().await {
                            Some(Ok(Server_Request::PublishService { info, service, responder })) => {
                                info!("Captured publish of GAS Service: {:?}", info);
                                gas_endpoints.service = Some(service.into_proxy());
                                let _ = responder.send(Ok(()));
                            }
                            x => error!("Got unexpected GAS Server request: {:?}", x),
                        }
                        gas_endpoints.gatt_server = Some(gatt_server);
                    }
                    Some(Ok(HostRequest::SetConnectable { responder, .. })) => {
                        info!("Setting connectable");
                        let _ = responder.send(Ok(()));
                    }
                    Some(Ok(HostRequest::SetBondingDelegate { delegate, .. })) => {
                        info!("Storing Bonding Delegate");
                        bonding_delegate = Some(delegate.into_stream());
                    }
                    Some(Ok(req)) => info!("Unhandled Host Request in add: {:?}", req),
                    Some(Err(e)) => error!("Error in host server: {:?}", e),
                    None => break,
                },
                bond_req = restore_bonds_fut => match bond_req {
                    Some(Ok(BondingDelegateRequest::RestoreBonds { bonds, responder })) => {
                        let rejected: Vec<sys::BondingData> = match &restore_bonds_behavior {
                            RestoreBondsBehavior::AcceptAll => Vec::new(),
                            RestoreBondsBehavior::RejectPeers(rejected_peers) => bonds
                                .into_iter()
                                .filter(|bond: &sys::BondingData| {
                                    bond.identifier
                                        .map(PeerId::from)
                                        .is_some_and(|id| rejected_peers.contains(&id))
                                })
                                .collect(),
                        };
                        responder.send(&rejected).expect("send RestoreBonds response");
                    }
                    other => panic!("expected RestoreBonds request, got: {other:?}"),
                },
            }
        }
        info!("Finishing host_device mocking for add host");
        (host_server, gas_endpoints, bonding_delegate.unwrap())
    }

    pub(crate) async fn create_and_add_test_host_to_dispatcher(
        id: HostId,
        dispatcher: &HostDispatcher,
    ) -> types::Result<(HostRequestStream, HostDevice, GasEndpoints, BondingDelegateRequestStream)>
    {
        create_and_add_test_host_with_bond_behavior(id, dispatcher, RestoreBondsBehavior::AcceptAll)
            .await
    }

    pub(crate) async fn create_and_add_test_host_with_bond_behavior(
        id: HostId,
        dispatcher: &HostDispatcher,
        restore_bonds_behavior: RestoreBondsBehavior,
    ) -> types::Result<(HostRequestStream, HostDevice, GasEndpoints, BondingDelegateRequestStream)>
    {
        let (host_server, host_device) = HostDevice::mock_from_id(id);
        let host_server_init_handler = handle_host_server_init(host_server, restore_bonds_behavior);
        let (res, (host_server, gas_endpoints, bonding_delegate)) =
            join(dispatcher.add_host_device(&host_device), host_server_init_handler).await;
        res?;
        Ok((host_server, host_device, gas_endpoints, bonding_delegate))
    }
}
