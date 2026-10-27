// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_helpers::maybe_stream::MaybeStream;
use async_utils::stream::FutureMap;
use bt_metrics::MetricsLogger;
use core::pin::Pin;
use core::task::{Context, Poll};
use fidl::endpoints::{ControlHandle, Proxy, RequestStream};
use fidl_fuchsia_bluetooth_sys::{
    AccessProxy, Error as SysError, InputCapability, OutputCapability, PairingDelegateMarker,
    PairingDelegateOnPairingRequestResponder as PairingResponder, PairingDelegateProxy,
    PairingDelegateRequest, PairingDelegateRequestStream, PairingMethod, PairingProxy,
};
use fuchsia_async::{self as fasync, DurationExt};
use fuchsia_bluetooth::types::{Address, Peer, PeerId};
use fuchsia_inspect::{self as inspect, Property};
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_inspect_derive::{AttachError, Inspect};

use futures::future::BoxFuture;
use futures::stream::{FusedStream, FuturesUnordered, Stream, StreamExt};
use futures::{Future, FutureExt};
use log::{debug, info, trace, warn};
use std::collections::HashMap;

use crate::types::packets::KeyBasedPairingAction;
use crate::types::{Error, SharedSecret};

pub struct PairingArgs {
    pub input: InputCapability,
    pub output: OutputCapability,
    pub delegate: PairingDelegateProxy,
}

/// The supported Fast Pair pairing methods.
/// See `KeyBasedPairingAction` for more details on each pairing type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PairingType {
    Initial,
    Subsequent,
    Retroactive { seeker_address: Address },
    PersonalizedNameWrite,
}

impl PairingType {
    pub fn from_action(action: &KeyBasedPairingAction, discoverable: bool) -> Self {
        use KeyBasedPairingAction::*;
        match action {
            SeekerInitiatesPairing | ProviderInitiatesPairing { .. } => {
                discoverable.then_some(Self::Initial).unwrap_or(Self::Subsequent)
            }
            PersonalizedNameWrite => Self::PersonalizedNameWrite,
            RetroactiveWrite { seeker_address } => {
                Self::Retroactive { seeker_address: *seeker_address }
            }
        }
    }
}

impl std::fmt::Display for PairingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Initial => "Initial",
            Self::Subsequent => "Subsequent",
            Self::Retroactive { .. } => "Retroactive",
            Self::PersonalizedNameWrite => "PersonalizedNameWrite",
        };
        write!(f, "{name}")
    }
}

/// The current owner of the Pairing Delegate.
#[derive(Debug, PartialEq)]
enum PairingDelegateOwner {
    /// The Fast Pair service owns the Pairing Delegate.
    FastPair,
    /// The upstream client owns the Pairing Delegate.
    Upstream,
}

impl PairingDelegateOwner {
    /// Returns true if Fast Pair currently owns the delegate.
    fn is_fast_pair(&self) -> bool {
        *self == Self::FastPair
    }
}

/// State of a Fast Pair pairing procedure.
#[derive(Debug)]
enum ProcedureState {
    /// The procedure has been initialized - e.g the peer has sent a key-based pairing GATT request.
    /// Waiting for the Seeker's passkey to identify and accept its in-progress pairing request.
    /// Entry state for `Initial` and `Subsequent` procedures.
    AwaitingPasskey,
    /// The Seeker's passkey was matched with an in-progress pairing request, which was accepted.
    /// Waiting for Classic/LE pairing to complete (`OnPairingComplete`).
    AwaitingPairingComplete,
    /// Classic/LE pairing was successfully completed.
    /// Waiting for the Seeker to write an Account Key.
    /// Entry state for `Retroactive` procedures.
    AwaitingAccountKey,
    /// Mandatory pairing steps are complete; waiting for a personalized name write before
    /// completing the procedure.
    /// Entry state for `PersonalizedNameWrite` procedures.
    AwaitingPersonalizedName,
    /// All steps of the pairing procedure have completed.
    Completed,
}

impl ProcedureState {
    fn initial(pairing_type: PairingType) -> Self {
        match pairing_type {
            PairingType::Initial | PairingType::Subsequent => Self::AwaitingPasskey,
            PairingType::Retroactive { .. } => Self::AwaitingAccountKey,
            PairingType::PersonalizedNameWrite => Self::AwaitingPersonalizedName,
        }
    }
}

#[derive(Default, Debug)]
struct ProcedureInspect {
    bredr_id: inspect::StringProperty,
    state: inspect::StringProperty,
    /// Expected to be a weak node.
    inspect_node: inspect::Node,
}

impl ProcedureInspect {
    fn new(le_id: &PeerId, inspect_node: inspect::Node) -> Self {
        inspect_node.record_string("le_id", format!("{le_id}"));
        let bredr_id = inspect_node.create_string("bredr_id", "");
        let state = inspect_node.create_string("state", "");
        Self { bredr_id, state, inspect_node }
    }
}

impl Drop for ProcedureInspect {
    fn drop(&mut self) {
        // Saves the "self" managed properties to be "automatically" managed.
        self.inspect_node.record(std::mem::take(&mut self.bredr_id));
        self.inspect_node.record(std::mem::take(&mut self.state));
    }
}

/// An active Fast Pair Pairing procedure.
struct Procedure {
    /// PeerId of the remote peer that we are currently pairing with. The PeerId is associated with
    /// the LE-version of the peer because the it always initiates Fast Pair over LE.
    le_id: PeerId,
    /// PeerId of the remote peer that we are currently pairing with. This is set during pairing if
    /// the peer initiates pairing over BR/EDR.
    bredr_id: Option<PeerId>,
    /// Shared secret used to encode/decode messages sent/received in the procedure.
    key: SharedSecret,
    /// The type of Fast Pair procedure.
    pairing_type: PairingType,
    /// Current status of the procedure.
    state: ProcedureState,
    /// Tracks the timeout of the pairing procedure.
    timer: Option<Pin<Box<fasync::Timer>>>,
    /// Inspect properties associated with this procedure.
    inspect_node: ProcedureInspect,
}

impl Procedure {
    /// Default timeout duration for a pairing procedure. Per the GFPS specification, if progress
    /// is not made within this amount of time, the procedure should be terminated.
    /// See https://developers.google.com/nearby/fast-pair/specifications/service/gatt#procedure
    const DEFAULT_PROCEDURE_TIMEOUT_DURATION: zx::MonotonicDuration =
        zx::MonotonicDuration::from_seconds(10);

    fn new(id: PeerId, key: SharedSecret, pairing_type: PairingType) -> Self {
        let timer =
            Box::pin(fasync::Timer::new(Self::DEFAULT_PROCEDURE_TIMEOUT_DURATION.after_now()));
        Self {
            le_id: id,
            bredr_id: None,
            key,
            pairing_type,
            state: ProcedureState::initial(pairing_type),
            timer: Some(timer),
            inspect_node: ProcedureInspect::default(),
        }
    }

    fn set_inspect(&mut self, node: ProcedureInspect) {
        self.inspect_node = node;
        self.inspect_node
            .bredr_id
            .set(&self.bredr_id.map_or_else(|| "".to_string(), |id| format!("{id}")));
        self.inspect_node.state.set(&format!("{:?}", self.state));
        self.inspect_node.inspect_node.record_string("type", format!("{}", self.pairing_type));
    }

    /// Moves the procedure to the new pairing `state` and resets the deadline for the procedure.
    fn transition(&mut self, state: ProcedureState) -> ProcedureState {
        let old_state = std::mem::replace(&mut self.state, state);
        self.inspect_node.state.set(&format!("{:?}", self.state));
        self.timer = Some(Box::pin(fasync::Timer::new(
            Self::DEFAULT_PROCEDURE_TIMEOUT_DURATION.after_now(),
        )));
        old_state
    }

    fn set_bredr_id(&mut self, id: PeerId) {
        self.bredr_id = Some(id);
        self.inspect_node.bredr_id.set(&format!("{id}"));
    }

    /// Returns true if the procedure is waiting for the Seeker's passkey to complete Classic/LE
    /// pairing.
    fn awaits_passkey(&self) -> bool {
        matches!(self.state, ProcedureState::AwaitingPasskey)
    }

    /// Returns true if the procedure requires the `sys.PairingDelegate` to complete Classic/LE
    /// pairing.
    fn awaits_pairing(&self) -> bool {
        matches!(
            self.state,
            ProcedureState::AwaitingPasskey | ProcedureState::AwaitingPairingComplete
        )
    }

    fn awaits_personalized_name(&self) -> bool {
        matches!(self.state, ProcedureState::AwaitingPersonalizedName)
    }
}

impl Future for Procedure {
    type Output = PeerId;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.timer {
            None => panic!("Future polled after completion (deadline reached)"),
            Some(timer) => match timer.poll_unpin(cx) {
                Poll::Ready(()) => {
                    trace!("Pairing procedure deadline reached: {:?}", self);
                    self.timer = None;
                    Poll::Ready(self.le_id)
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl std::fmt::Debug for Procedure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Procedure")
            .field("le_id", &self.le_id)
            .field("bredr_id", &self.bredr_id)
            .field("pairing_type", &self.pairing_type)
            .field("state", &self.state)
            .field("timer", &self.timer)
            .finish()
    }
}

/// A Classic/LE pairing request that has not yet been matched to a Fast Pair procedure.
///
/// A pairing request can't be reliably attributed to a procedure when it is received. For
/// example, the Seeker typically initiates Fast Pair over LE but makes the pairing request over
/// BR/EDR, in which case the system assigns a different PeerId to each.
/// Therefore, a pairing request is held until a Seeker proves ownership of it by sending the
/// matching passkey over the encrypted GATT channel.
///
/// The request is rejected if it is dropped before being accepted.
struct PendingPairingRequest {
    /// PeerId of the peer that made the pairing request.
    id: PeerId,
    /// The passkey of the pairing request, as reported by the system.
    passkey: u32,
    /// Used to respond to the pairing request. `None` once a response has been sent.
    responder: Option<PairingResponder>,
    /// Tracks the timeout of the pairing request.
    timer: Option<Pin<Box<fasync::Timer>>>,
}

impl PendingPairingRequest {
    fn new(id: PeerId, passkey: u32, responder: PairingResponder) -> Self {
        let timer =
            Box::pin(fasync::Timer::new(Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION.after_now()));
        Self { id, passkey, responder: Some(responder), timer: Some(timer) }
    }

    /// Accepts the pairing request.
    fn accept(&mut self) {
        if let Some(responder) = self.responder.take() {
            let _ = responder.send(true, self.passkey);
        }
    }

    /// Rejects the pairing request.
    fn reject(&mut self) {
        if let Some(responder) = self.responder.take() {
            let _ = responder.send(false, 0);
        }
    }
}

impl Future for PendingPairingRequest {
    type Output = PeerId;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.timer {
            // The deadline was already reached.
            None => Poll::Ready(self.id),
            Some(timer) => match timer.poll_unpin(cx) {
                Poll::Ready(()) => {
                    self.timer = None;
                    Poll::Ready(self.id)
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl Drop for PendingPairingRequest {
    fn drop(&mut self) {
        // Reject the pairing request if it was never accepted.
        self.reject();
    }
}

impl std::fmt::Debug for PendingPairingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPairingRequest").field("id", &self.id).finish()
    }
}

/// The maximum number of recently completed procedures that are tracked in Inspect.
pub const MAX_SAVED_PROCEDURES: usize = 5;

/// The maximum number of pairing requests that can be waiting to be matched to a Fast Pair
/// procedure.
const MAX_PENDING_PAIRING_REQUESTS: usize = 5;

/// An inspect node tracking a Fast Pair procedure that is in progress.
struct ActiveProcedureInspect {
    /// The time the procedure was started at.
    start_time: fasync::MonotonicInstant,
    inspect_node: inspect::Node,
}

#[derive(Default)]
struct PairingManagerInspect {
    owner: inspect::StringProperty,
    finished_procedures: Option<BoundedListNode>,
    active_procedures: HashMap<PeerId, ActiveProcedureInspect>,
    inspect_node: inspect::Node,
    /// Used to log component metrics via Cobalt.
    metrics: MetricsLogger,
}

impl Inspect for &mut PairingManagerInspect {
    fn iattach(self, parent: &inspect::Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.inspect_node = parent.create_child(name.as_ref());
        self.owner = self.inspect_node.create_string("owner", "");
        self.finished_procedures = Some(BoundedListNode::new(
            self.inspect_node.create_child("finished_procedures"),
            MAX_SAVED_PROCEDURES,
        ));
        Ok(())
    }
}

impl PairingManagerInspect {
    fn with_metrics(mut self, metrics: MetricsLogger) -> Self {
        self.metrics = metrics;
        self
    }

    fn set_owner(&self, owner: &PairingDelegateOwner) {
        match owner {
            PairingDelegateOwner::FastPair => self.owner.set("FastPair"),
            PairingDelegateOwner::Upstream => self.owner.set("Upstream"),
        }
    }

    /// Creates an inspect node for the Fast Pair pairing procedure. Returns the Inspect node which
    /// can be used to record information about the active procedure.
    pub fn record_new_procedure(&mut self, id: PeerId, name: &str) -> ProcedureInspect {
        let node = self.inspect_node.create_child(name);
        // The inspect node for procedure is owned by `PairingManagerInspect`. However, the
        // procedure can make modifications via a weak clone of the node.
        let procedure_node = ProcedureInspect::new(&id, node.clone_weak());
        let _ = self.active_procedures.insert(
            id,
            ActiveProcedureInspect {
                start_time: fasync::MonotonicInstant::now(),
                inspect_node: node,
            },
        );
        procedure_node
    }

    /// Saves the active pairing procedure to the inspect hierarchy.
    pub fn record_finished_procedure(&mut self, id: PeerId, at: fasync::MonotonicInstant) {
        let Some(bounded_list_node) = self.finished_procedures.as_mut() else { return };
        let Some(ActiveProcedureInspect { start_time, inspect_node }) =
            self.active_procedures.remove(&id)
        else {
            return;
        };
        let pairing_time_seconds = (at - start_time).into_seconds().try_into().unwrap_or_default();
        inspect_node.record_uint("pairing_time_seconds", pairing_time_seconds);
        let _ = bounded_list_node.add_entry(|node| {
            let _ = node.adopt(&inspect_node);
            let _ = node.record(inspect_node);
        });
    }
}

/// The `PairingManager` is responsible for servicing `sys.Pairing` requests.
///
/// Because pairing can't occur unless a Pairing Delegate is set, the `PairingManager` is
/// constructed with the upstream client's `sys.Pairing` connection. Therefore, the lifetime of the
/// `PairingManager` is tied to that of the upstream client and the downstream connection to the
/// Pairing Delegate.
///
/// The `PairingManager` handles pairing events related to Fast Pair and forwards unrelated events
/// directly to the upstream `sys.Pairing` client. Only one Pairing Delegate can be active at a
/// time. Because Fast Pair and the upstream client may require different I/O capabilities, the
/// `PairingManager` unregisters and re-registers the delegate any time there is a change in
/// ownership.
///
/// The `PairingManager` supports the handling of Fast Pair pairing procedures with multiple peers.
/// However, there can only be one active procedure per remote peer. When the mandatory steps of a
/// pairing procedure complete (e.g. `Procedure::awaits_personalized_name`), it will remain in the
/// set of active `procedures` until either explicitly removed via
/// `PairingManager::complete_pairing_procedure` or evicted after meeting the time deadline.
///
/// Pairing requests received while Fast Pair owns the Pairing Delegate are not immediately
/// associated with a procedure. The request may come from a different PeerId than the one that
/// initiated Fast Pair (e.g. LE vs BR/EDR), or from an unrelated peer entirely. Instead, requests
/// are held as pending and a request is only accepted once a Seeker provides its passkey over the
/// encrypted GATT channel (`PairingManager::compare_passkey`) and that passkey matches the request.
/// Pending requests that aren't matched within the procedure timeout are rejected.
///
/// If the system reports a successful pairing with a peer whose procedure never matched the
/// Seeker's passkey, the resulting bond is unauthenticated and is removed via `sys.Access`.
///
/// Note: Due to a limitation of the `sys.Pairing` API, any non-Fast Pair pairing request made while
/// Fast Pair owns the Pairing Delegate will incorrectly use Fast Pair I/O capabilities. Ideally,
/// we'd like the granularity of I/O capabilities to be at the peer level. To avoid confusion at
/// the upstream level, the `PairingManager` will reject such pairing requests while Fast Pair is
/// active. See https://fxbug.dev/42052578 for more details.
///
/// PairingManager implements `Stream`. Clients should poll this stream to receive updates about
/// Fast Pair pairing.
pub struct PairingManager {
    /// Connection to the `sys.Pairing` protocol.
    pairing: PairingProxy,
    /// Connection to the `sys.Access` protocol. Used to remove unauthenticated bonds.
    access: AccessProxy,
    /// Upstream client that has ownership of the Pairing Delegate.
    upstream_client: PairingArgs,
    /// This future resolves when the upstream client closes its Pairing Delegate.
    upstream_connection: BoxFuture<'static, ()>,
    /// The current owner of the downstream connection to the Pairing Delegate.
    owner: PairingDelegateOwner,
    /// The downstream connection to the Pairing Delegate.
    /// This is an optional stream because the PairingManager resets the delegate any time there is
    /// a change in the delegate `owner`.
    pairing_requests: MaybeStream<PairingDelegateRequestStream>,
    /// Active tasks relaying a request from the downstream Pairing Delegate to the upstream client.
    relay_tasks: FuturesUnordered<BoxFuture<'static, ()>>,
    /// Active tasks removing unauthenticated bonds via `sys.Access/Forget`.
    forget_tasks: FuturesUnordered<BoxFuture<'static, ()>>,
    /// Active pairing procedures. Each pairing procedure is identified by the peer's LE-discovered
    /// PeerId.
    procedures: FutureMap<PeerId, Procedure>,
    /// Pairing requests waiting to be matched to a procedure via the Seeker's passkey. Each
    /// request is identified by the PeerId that made it.
    pending_requests: FutureMap<PeerId, PendingPairingRequest>,
    /// Peers with an in-progress non-Fast Pair pairing request relayed to the upstream client.
    /// Maps the peer's `PeerId` to its Bluetooth `Address` until `OnPairingComplete` is received.
    upstream_pairing_peers: HashMap<PeerId, Address>,
    /// Peers that recently completed non-Fast Pair pairing and are eligible to perform a single
    /// retroactive Account Key write within `Self::DEFAULT_RETROACTIVE_WRITE_TIMEOUT_DURATION`.
    retroactive_pairing_peers: FutureMap<Address, fasync::Timer>,
    /// If the PairingManager is finished - i.e either the upstream or downstream delegate
    /// connection has terminated.
    terminated: bool,
    /// Inspect properties associated with the PairingManager.
    inspect_node: PairingManagerInspect,
}

impl Inspect for &mut PairingManager {
    fn iattach(self, parent: &inspect::Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.inspect_node.iattach(parent, name.as_ref())?;
        self.inspect_node.set_owner(&self.owner);
        Ok(())
    }
}

impl PairingManager {
    /// Default timeout duration for a retroactive account key write after non-Fast Pair pairing
    /// completes. Per the GFPS specification, a Seeker may retroactively write an Account Key for
    /// up to 1 minute after bonding completes.
    /// See https://developers.google.com/nearby/fast-pair/specifications/extensions/retroactiveacctkey
    const DEFAULT_RETROACTIVE_WRITE_TIMEOUT_DURATION: zx::MonotonicDuration =
        zx::MonotonicDuration::from_seconds(60);

    pub fn new(
        pairing: PairingProxy,
        access: AccessProxy,
        upstream_client: PairingArgs,
        metrics: MetricsLogger,
    ) -> Result<Self, Error> {
        let delegate = upstream_client.delegate.clone();
        let upstream_connection = async move {
            let _ = delegate.on_closed().await;
        }
        .boxed();
        let mut this = Self {
            pairing,
            access,
            upstream_client,
            upstream_connection,
            pairing_requests: MaybeStream::default(),
            owner: PairingDelegateOwner::Upstream,
            relay_tasks: FuturesUnordered::new(),
            forget_tasks: FuturesUnordered::new(),
            procedures: FutureMap::new(),
            pending_requests: FutureMap::new(),
            upstream_pairing_peers: HashMap::new(),
            retroactive_pairing_peers: FutureMap::new(),
            terminated: false,
            inspect_node: PairingManagerInspect::default().with_metrics(metrics),
        };
        this.set_pairing_delegate(PairingDelegateOwner::Upstream)?;
        Ok(this)
    }

    /// Notifies the upstream `sys.Pairing` client of Fast Pair completion.
    pub fn notify_pairing_complete(&self, id: PeerId) {
        if let Err(e) =
            self.upstream_client.delegate.on_pairing_complete(&id.into(), /* success */ true)
        {
            warn!(id:%; "Couldn't notify `sys.Pairing` client: {e:}");
        }
    }

    /// Attempts to set the downstream Pairing Delegate.
    ///
    /// Note: This is an internal method. To avoid unnecessary churn, it should only be used when
    /// there is a change in ownership of the delegate.
    ///
    /// Returns Error if the delegate couldn't be set.
    fn set_pairing_delegate(&mut self, owner: PairingDelegateOwner) -> Result<(), Error> {
        self.close_delegate();

        let (input, output) = match owner {
            PairingDelegateOwner::FastPair => {
                (InputCapability::Confirmation, OutputCapability::Display)
            }
            PairingDelegateOwner::Upstream => {
                (self.upstream_client.input, self.upstream_client.output)
            }
        };

        let (c, s) = fidl::endpoints::create_request_stream::<PairingDelegateMarker>();
        self.pairing.set_pairing_delegate(input, output, c)?;
        self.pairing_requests.set(s);
        self.inspect_node.set_owner(&owner);
        self.owner = owner;
        Ok(())
    }

    fn close_delegate(&mut self) {
        // Pending pairing requests are associated with the current delegate and are rejected.
        self.clear_pending_requests();
        self.upstream_pairing_peers.clear();
        if let Some(stream) = MaybeStream::take(&mut self.pairing_requests) {
            stream.control_handle().shutdown();
        }
    }

    /// Rejects and removes all pending pairing requests.
    fn clear_pending_requests(&mut self) {
        // Dropping a `PendingPairingRequest` rejects it.
        self.pending_requests.inner().clear();
    }

    /// Rejects and removes all pending pairing requests if no procedure can claim them.
    fn maybe_clear_pending_requests(&mut self) {
        if !self.procedures.inner().values().any(|p| p.awaits_passkey()) {
            self.clear_pending_requests();
        }
    }

    /// Attempts to claim the Pairing Delegate for Fast Pair use.
    /// No-op if the delegate is already owned by Fast Pair.
    /// Returns Error if the delegate was unable to be set.
    fn claim_delegate(&mut self) -> Result<(), Error> {
        // While unusual, this typically signifies another ongoing Fast Pair procedure - probably
        // with a different peer.
        if self.owner.is_fast_pair() {
            return Ok(());
        }

        // Give pairing capabilities to Fast Pair.
        self.set_pairing_delegate(PairingDelegateOwner::FastPair)
    }

    /// Releases the Pairing Delegate back to the upstream client.
    /// No-op if the delegate is not currently owned by Fast Pair or if there are Fast Pair
    /// procedures still awaiting Classic/LE pairing.
    /// Returns Error if the operation couldn't be completed.
    fn release_delegate(&mut self) -> Result<(), Error> {
        if !self.owner.is_fast_pair()
            || self.procedures.inner().values().any(|p| p.awaits_pairing())
        {
            return Ok(());
        }

        // Give pairing capabilities back to the upstream client.
        self.set_pairing_delegate(PairingDelegateOwner::Upstream)
    }

    /// Returns the SharedSecret for the active procedure with the remote peer.
    pub fn key_for_procedure(&mut self, le_id: &PeerId) -> Option<&SharedSecret> {
        self.procedures.inner().get(le_id).map(|procedure| &procedure.key)
    }

    /// Returns the LE `PeerId` (the `procedures` key) and the procedure that `le_or_bredr_id`
    /// belongs to, either directly or via a BR/EDR `PeerId` bound in `Self::compare_passkey`.
    fn find_procedure(&mut self, le_or_bredr_id: PeerId) -> Option<(PeerId, &mut Procedure)> {
        self.procedures
            .inner()
            .iter_mut()
            .find(|(le_id, p)| **le_id == le_or_bredr_id || p.bredr_id == Some(le_or_bredr_id))
            .map(|(le_id, p)| (*le_id, &mut **p))
    }

    fn new_procedure(&mut self, le_id: PeerId, key: SharedSecret, pairing_type: PairingType) {
        let mut procedure = Procedure::new(le_id, key, pairing_type);
        procedure.set_inspect(
            self.inspect_node.record_new_procedure(le_id, &inspect::unique_name("procedure_")),
        );
        let _ = self.procedures.insert(le_id, procedure);
    }

    /// Attempts to initiate a new Fast Pair pairing procedure with the provided peer.
    pub fn new_pairing_procedure(
        &mut self,
        le_id: PeerId,
        key: SharedSecret,
        pairing_type: PairingType,
    ) -> Result<(), Error> {
        debug!(le_id:%; "Starting a new pairing procedure ({pairing_type:?})");
        if self.procedures.contains_key(&le_id) {
            return Err(Error::internal(&format!("Pairing with {le_id} already in progress")));
        }

        match pairing_type {
            PairingType::Initial | PairingType::Subsequent => {
                self.claim_delegate()?;
            }
            // Checks for recently connected non-fast pair peers
            PairingType::Retroactive { seeker_address } => {
                if self.retroactive_pairing_peers.remove(&seeker_address).is_none() {
                    return Err(Error::internal(&format!(
                        "Peer ({seeker_address}) is not eligible for retroactive pairing"
                    )));
                }
            }
            PairingType::PersonalizedNameWrite => {}
        }

        self.new_procedure(le_id, key, pairing_type);
        Ok(())
    }

    /// Matches `gatt_passkey`, received over LE (GATT) from the Seeker (`le_id`), with a pending
    /// pairing request (made over LE or BR/EDR) from the `sys.PairingDelegate` (see
    /// `Self::handle_pairing_request`).
    ///
    /// Returns:
    /// - `Ok` with the Provider's passkey (from the Seeker's pending request):
    ///   1. If a single pending request matches `gatt_passkey`. The request is accepted (GFPS
    ///      Step 13.1) and bound to the Seeker's procedure.
    ///   2. If no pending request matches `gatt_passkey`, but the Seeker's procedure is the only
    ///      one awaiting pairing and only one request is pending, allowing the Provider to notify
    ///      its passkey (GFPS Step 14). The request is rejected (Step 13.2) and the procedure is
    ///      canceled (Step 16.2).
    /// - `Error` if the Seeker's procedure isn't awaiting pairing, if multiple requests match (all
    ///   matching requests are rejected), or if none match and the Seeker's request can't be
    ///   unambiguously identified.
    // TODO(https://fxbug.dev/42053956): There is an implicit assumption that the peer has already made the
    // pairing request before `compare_passkey` is called with the GATT passkey. While the GFPS
    // does specify this ordering, this may not always be the case in practice.
    pub fn compare_passkey(&mut self, le_id: PeerId, gatt_passkey: u32) -> Result<u32, Error> {
        debug!(le_id:%, gatt_passkey:%; "Comparing passkey");
        if !self.procedures.inner().get(&le_id).is_some_and(|p| p.awaits_passkey()) {
            return Err(Error::internal(&format!("Unexpected passkey response for {le_id}")));
        }

        // Find pending pairing requests matching `gatt_passkey`.
        let matching_ids: Vec<PeerId> = self
            .pending_requests
            .inner()
            .iter()
            .filter(|(_id, request)| request.passkey == gatt_passkey)
            .map(|(id, _request)| *id)
            .collect();
        let request_id = match matching_ids.as_slice() {
            [id] => *id,
            // Passkey mismatch. If this is the only procedure awaiting pairing and only one
            // request is pending, the Seeker's request is unambiguous: reject it (GFPS Step 13.2;
            // also preventing its disclosed passkey from being used to claim it), cancel the
            // procedure (Step 16.2), and return its passkey to be notified (Step 14).
            [] => {
                let sole_procedure =
                    self.procedures.inner().values().filter(|p| p.awaits_passkey()).count() == 1;
                let sole_request = if self.pending_requests.inner().len() == 1 {
                    // Grab the known single pending pairing request.
                    self.pending_requests.inner().keys().next().copied()
                } else {
                    None
                };
                let (true, Some(request_id)) = (sole_procedure, sole_request) else {
                    return Err(Error::internal(&format!(
                        "No pairing request matches the passkey for {le_id}"
                    )));
                };
                warn!(le_id:%, request_id:%; "Passkey mismatch. Canceling Fast Pair procedure");
                let mut request = self.pending_requests.remove(&request_id).expect("just found");
                request.reject();
                if let Err(e) = self.cancel_pairing_procedure(&le_id) {
                    warn!(le_id:%, e:?; "Couldn't release Pairing Delegate");
                }
                return Ok(request.passkey);
            }
            // Passkey collision: the Seeker's request is ambiguous. Reject all matching requests.
            _ => {
                warn!(le_id:%, count:% = matching_ids.len(); "Multiple pairing requests match passkey");
                for id in &matching_ids {
                    let _ = self.pending_requests.remove(id);
                }
                return Err(Error::internal(&format!(
                    "Multiple pairing requests match the passkey for {le_id}"
                )));
            }
        };

        let mut request = self.pending_requests.remove(&request_id).expect("just found");
        let procedure = self.procedures.inner().get_mut(&le_id).expect("just checked");
        let _ = procedure.transition(ProcedureState::AwaitingPairingComplete);
        // The pairing request shares the Seeker's LE PeerId if it was made over LE, or if the
        // system already associates the peer's LE and BR/EDR identities.
        if request_id != le_id {
            procedure.set_bredr_id(request_id);
        }
        request.accept();
        // Remaining pending requests are rejected if there are no other procedures to claim them.
        self.maybe_clear_pending_requests();
        Ok(gatt_passkey)
    }

    /// Progresses the procedure after an Account Key has been written.
    /// Returns Error if there is no active procedure in the correct state.
    // TODO(https://fxbug.dev/42053956): There is an implicit assumption that the peer has already
    // successfully completed the Classic/LE pairing request (e.g OnPairingComplete
    // { success = true }) before `account_key_write` is called. While the GFPS does specify this
    // ordering, this may not always be the case in practice.
    pub fn account_key_write(&mut self, le_id: PeerId) -> Result<(), Error> {
        debug!(le_id:%; "Processing account key write");
        let procedure = self
            .procedures
            .inner()
            .get_mut(&le_id)
            .filter(|p| matches!(p.state, ProcedureState::AwaitingAccountKey))
            .ok_or_else(|| {
                Error::internal(&format!("Procedure with {le_id} is not in the correct state"))
            })?;
        let _ = procedure.transition(ProcedureState::AwaitingPersonalizedName);
        Ok(())
    }

    /// Completes the pairing procedure with the peer, saving its inspect data and cleaning up its
    /// state.
    /// Returns Error if there is no such finished procedure or if cleanup failed.
    pub fn complete_pairing_procedure(&mut self, le_id: PeerId) -> Result<(), Error> {
        let procedure = self
            .procedures
            .inner()
            .get_mut(&le_id)
            .filter(|p| p.awaits_personalized_name())
            .ok_or_else(|| {
                Error::internal(&format!("Procedure with {le_id} is not in the correct state"))
            })?;
        let _ = procedure.transition(ProcedureState::Completed);

        // Procedure is in the correct (finished) state and we can save the inspect data and clean
        // up the procedure.
        self.inspect_node.record_finished_procedure(le_id, fasync::MonotonicInstant::now());
        self.cancel_pairing_procedure(&le_id)
    }

    /// Cancels the Fast Pair pairing procedure with the peer.
    /// Returns Ok if the PairingDelegate was successfully released, or Error otherwise.
    pub fn cancel_pairing_procedure(&mut self, id: &PeerId) -> Result<(), Error> {
        let _ = self.procedures.remove(id);
        self.maybe_clear_pending_requests();
        self.release_delegate()
    }

    /// Removes the bond with the peer from the system via `sys.Access/Forget`.
    fn forget_peer(&mut self, id: PeerId) {
        let response = self.access.forget(&id.into());
        let task = async move {
            match response.await {
                Ok(Ok(())) | Ok(Err(SysError::PeerNotFound)) => {}
                Ok(Err(e)) => warn!(id:%; "Couldn't forget unauthenticated peer: {e:?}"),
                Err(e) => warn!(id:%; "FIDL error when forgetting unauthenticated peer: {e:?}"),
            }
        };
        self.forget_tasks.push(task.boxed());
    }

    async fn proxy_pairing_request(request: PairingDelegateRequest, proxy: PairingDelegateProxy) {
        match request {
            PairingDelegateRequest::OnPairingRequest {
                peer,
                method,
                displayed_passkey,
                responder,
            } => {
                let (accept, entered_passkey) =
                    match proxy.on_pairing_request(&peer, method, displayed_passkey).await {
                        Ok((a, p)) => (a, p),
                        Err(e) => {
                            warn!("FIDL error when handling OnPairingRequest: {:?}", e);
                            (false, 0)
                        }
                    };
                let _ = responder.send(accept, entered_passkey);
            }
            PairingDelegateRequest::OnPairingComplete { id, success, .. } => {
                let _ = proxy.on_pairing_complete(&id, success);
            }
            PairingDelegateRequest::OnRemoteKeypress { id, keypress, .. } => {
                let _ = proxy.on_remote_keypress(&id, keypress);
            }
        }
    }

    /// Handles an incoming pairing request from the `sys.PairingDelegate`.
    ///
    /// A Seeker initiates Fast Pair over LE (GATT), but the pairing request may be made over
    /// either LE or BR/EDR. In the BR/EDR case, the system may assign two different `PeerId`s
    /// to the same peer until bonding completes. As a result, `le_or_bredr_id` cannot be
    /// reliably matched to an active `Procedure` here.
    ///
    /// Instead, valid requests are held in `pending_requests`, and procedure matching is deferred
    /// to `Self::compare_passkey` when the Seeker sends the matching passkey over GATT.
    fn handle_pairing_request(
        &mut self,
        le_or_bredr_id: PeerId,
        method: PairingMethod,
        passkey: u32,
        responder: PairingResponder,
    ) -> Result<(), Error> {
        // Unsupported pairing methods will always be rejected. Any active Fast Pair procedure with
        // the peer (matched by its LE or BR/EDR PeerId) is canceled.
        if method != PairingMethod::PasskeyComparison {
            warn!(le_or_bredr_id:?, method:?; "Received unsupported pairing method");
            let _ = responder.send(false, 0u32);
            if let Some((le_id, _)) = self.find_procedure(le_or_bredr_id) {
                self.cancel_pairing_procedure(&le_id)?;
            }
            return Ok(());
        }

        // There is an active Fast Pair procedure with the peer (matched by its LE or BR/EDR
        // PeerId), but it is not waiting for a pairing request. Invalidates the current pairing
        // procedure and rejects the pairing request.
        if let Some((le_id, procedure)) =
            self.find_procedure(le_or_bredr_id).filter(|(_, p)| !p.awaits_passkey())
        {
            warn!(
                le_or_bredr_id:%, le_id:%, procedure_state:? = procedure.state;
                "Received unexpected pairing request"
            );
            let _ = responder.send(false, 0u32);
            self.cancel_pairing_procedure(&le_id)?;
            return Ok(());
        }

        // There is no Fast Pair procedure awaiting pairing (i.e. a procedure in the
        // `AwaitingPasskey` state). Rejects the pairing request.
        if !self.procedures.inner().values().any(|p| p.awaits_passkey()) {
            warn!(
                le_or_bredr_id:%;
                "Couldn't match pairing request with inflight Fast Pair procedure"
            );
            // TODO(https://fxbug.dev/42052578): Consider relaying upstream if I/O capabilities are
            // defined per peer.
            let _ = responder.send(false, 0u32);
            return Ok(());
        }

        // A newer request from the same peer replaces any older one, which is rejected.
        let _ = self.pending_requests.remove(&le_or_bredr_id);

        // The number of pending pairing requests is bounded by `MAX_PENDING_PAIRING_REQUESTS`.
        // New pairing requests are rejected until an existing pending request is matched,
        // canceled, or expires.
        if self.pending_requests.inner().len() >= MAX_PENDING_PAIRING_REQUESTS {
            warn!(le_or_bredr_id:%; "Too many pending pairing requests. Rejecting");
            let _ = responder.send(false, 0u32);
            return Ok(());
        }

        let _ = self
            .pending_requests
            .insert(le_or_bredr_id, PendingPairingRequest::new(le_or_bredr_id, passkey, responder));
        Ok(())
    }

    fn handle_pairing_complete(
        &mut self,
        le_or_bredr_id: PeerId,
        success: bool,
    ) -> Result<Option<PeerId>, Error> {
        debug!(le_or_bredr_id:%, success; "pairing complete");
        // Pairing with the peer has finished, so any pending request from it is no longer valid.
        let was_pending = self.pending_requests.remove(&le_or_bredr_id).is_some();
        // Try to match the request to the peer's LE or BR/EDR PeerId.
        let Some((le_id, procedure)) = self.find_procedure(le_or_bredr_id) else {
            if was_pending {
                if success {
                    // Pending requests have not been accepted by Fast Pair, so the resulting bond
                    // is unauthenticated and is removed.
                    warn!(
                        le_or_bredr_id:%;
                        "Pairing completed for pending request without Fast Pair authentication. Removing bond"
                    );
                    self.forget_peer(le_or_bredr_id);
                } else {
                    debug!(le_or_bredr_id:%; "Pending pairing request failed before Fast Pair authentication");
                }
            } else {
                // Fast Pair never handled this peer. This can happen when the upstream client
                // accepted the peer's pairing request shortly before Fast Pair claimed the
                // delegate.
                // Relay the outcome so the upstream client can observe it.
                debug!(
                    le_or_bredr_id:%, success;
                    "Pairing complete for non-Fast Pair peer. Relaying upstream"
                );
                if let Err(e) = self
                    .upstream_client
                    .delegate
                    .on_pairing_complete(&le_or_bredr_id.into(), success)
                {
                    warn!(le_or_bredr_id:%; "Couldn't relay OnPairingComplete upstream: {e:?}");
                }
            }
            return Ok(None);
        };

        if !success {
            info!(le_id:%; "Pairing failure");
            self.cancel_pairing_procedure(&le_id)?;
            return Ok(None);
        }

        match procedure.state {
            // The pairing request was accepted after the Seeker's passkey was matched.
            ProcedureState::AwaitingPairingComplete => {
                let _ = procedure.transition(ProcedureState::AwaitingAccountKey);
                self.release_delegate()?;
                Ok(Some(le_id))
            }
            // Pairing requests are only accepted in Fast Pair once the Seeker's passkey is matched.
            // The resulting bond is not spec-compliant and is removed.
            ProcedureState::AwaitingPasskey => {
                // a procedure is always created through LE and can be matched using the LE PeerId
                debug_assert_eq!(le_id, le_or_bredr_id);
                warn!(le_id:%; "Pairing completed without Fast Pair authentication. Removing bond");
                self.forget_peer(le_id);
                self.cancel_pairing_procedure(&le_id)?;
                Ok(None)
            }
            // Pairing was already completed - the bond is either authenticated through passkey
            // comparison or is a Retroactive or PersonalizedNameWrite request.
            ProcedureState::AwaitingAccountKey
            | ProcedureState::AwaitingPersonalizedName
            | ProcedureState::Completed => {
                warn!(le_id:%, procedure_state:? = procedure.state; "Unexpected pairing success for Fast Pair procedure");
                self.cancel_pairing_procedure(&le_id)?;
                Ok(None)
            }
        }
    }

    fn handle_pairing_delegate_request(
        &mut self,
        request: PairingDelegateRequest,
    ) -> Result<Option<PeerId>, Error> {
        debug!(request:?; "Received PairingDelegate request");
        if !self.owner.is_fast_pair() {
            debug!("Relaying PairingDelegate request to upstream");
            match &request {
                PairingDelegateRequest::OnPairingRequest { peer, .. } => {
                    let peer = Peer::try_from(peer.clone())?;
                    let _ = self.upstream_pairing_peers.insert(peer.id, peer.address);
                }
                PairingDelegateRequest::OnPairingComplete { id, success, .. } => {
                    let id = (*id).into();
                    if let Some(address) = self.upstream_pairing_peers.remove(&id) {
                        if *success {
                            let timer = fasync::Timer::new(
                                Self::DEFAULT_RETROACTIVE_WRITE_TIMEOUT_DURATION.after_now(),
                            );
                            let _ = self.retroactive_pairing_peers.insert(address, timer);
                        } else {
                            let _ = self.retroactive_pairing_peers.remove(&address);
                        }
                    }
                }
                PairingDelegateRequest::OnRemoteKeypress { .. } => {}
            }
            let relay_task =
                Self::proxy_pairing_request(request, self.upstream_client.delegate.clone()).boxed();
            self.relay_tasks.push(relay_task);
            return Ok(None);
        }

        match request {
            PairingDelegateRequest::OnPairingRequest {
                peer,
                method,
                displayed_passkey,
                responder,
            } => {
                let peer = Peer::try_from(peer)?;
                self.handle_pairing_request(peer.id, method, displayed_passkey, responder)?;
            }
            PairingDelegateRequest::OnPairingComplete { id, success, .. } => {
                return self.handle_pairing_complete(id.into(), success);
            }
            PairingDelegateRequest::OnRemoteKeypress { .. } => {}
        }
        Ok(None)
    }

    fn poll_procedure_deadlines(&mut self, cx: &mut Context<'_>) {
        if self.procedures.is_terminated() {
            return;
        }

        if let Poll::Ready(Some(id)) = self.procedures.poll_next_unpin(cx) {
            info!(id:%; "Deadline reached for Fast Pair procedure. Canceling");
            // Deadline was reached (e.g no updates within the expected time). Procedure is no
            // longer active. In most cases, this signifies an end to the "mandatory" part of the
            // Fast Pair flow. As such, the inspect data is saved.
            self.inspect_node.record_finished_procedure(id, fasync::MonotonicInstant::now());
            let _ = self.cancel_pairing_procedure(&id);
        }
    }

    fn poll_pending_request_deadlines(&mut self, cx: &mut Context<'_>) {
        if self.pending_requests.is_terminated() {
            return;
        }

        // The expired request is removed from `pending_requests` and rejected when dropped.
        while let Poll::Ready(Some(id)) = self.pending_requests.poll_next_unpin(cx) {
            info!(id:%; "Pairing request wasn't matched with a Fast Pair procedure in time");
        }
    }

    fn poll_retroactive_pairing_deadlines(&mut self, cx: &mut Context<'_>) {
        if self.retroactive_pairing_peers.is_terminated() {
            return;
        }

        while let Poll::Ready(Some(())) = self.retroactive_pairing_peers.poll_next_unpin(cx) {
            trace!("Retroactive pairing window expired for peer");
        }
    }
}

/// The Stream implementation for `PairingManager` does 7 things:
///   (1) Poll the connection to the upstream `sys.Pairing` client to check if it is open. Because
///       pairing functionality is determined by the client's existence, the `PairingManager` stream
///       will terminate if the client disconnects.
///   (2) Drive pairing relay futures to completion. This is applicable to non-Fast Pair requests.
///       Relaying the request to the upstream `sys.Pairing` client is async. Each request is
///       inserted into a collection and is progressed anytime the stream is polled.
///   (3) Poll the connection to the downstream `sys.Pairing` delegate. Received pairing requests
///       will be handled by the `PairingManager`.
///   (4) Drive `sys.Access/Forget` futures, which remove unauthenticated bonds, to completion.
///   (5) Poll the set of active Fast Pair pairing procedures. Per the GFPS, if a procedure doesn't
///       progress within `Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION`, the shared secret `key` is no
///       longer valid.
///   (6) Poll the set of pending pairing requests. Requests that aren't matched to a procedure
///       within `Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION` are rejected.
///   (7) Poll the set of peers eligible for retroactive Account Key writes, evicting entries that
///       exceed `PairingManager::DEFAULT_RETROACTIVE_WRITE_TIMEOUT_DURATION`.
/// The Stream produces an item anytime Fast Pair pairing successfully completes with a remote peer.
impl Stream for PairingManager {
    type Item = PeerId;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_terminated() {
            panic!("Cannot poll a terminated stream");
        }

        // Check if upstream client is still there.
        if let Poll::Ready(_) = self.upstream_connection.poll_unpin(cx) {
            trace!("Upstream client disconnected. PairingManager stream is terminated.");
            self.terminated = true;
            return Poll::Ready(None);
        }

        // Drive any active relay tasks to completion.
        if !self.relay_tasks.is_terminated() {
            let _ = Pin::new(&mut self.relay_tasks).poll_next(cx);
        }

        // Check for any new pairing requests from the downstream Pairing Delegate.
        while let Poll::Ready(result) = self.pairing_requests.poll_next_unpin(cx) {
            match result {
                Some(Ok(request)) => match self.handle_pairing_delegate_request(request) {
                    Ok(None) => continue,
                    Ok(event) => return Poll::Ready(event),
                    Err(e) => {
                        warn!("Error handling PairingDelegate FIDL request: {:?}", e);
                    }
                },
                Some(Err(e)) => {
                    warn!("Error in PairingDelegate FIDL channel: {}", e);
                }
                None => (),
            }

            // Either the `PairingDelegate` stream is exhausted, or there is an error in the
            // channel.
            trace!("Downstream channel closed. PairingManager stream is terminated.");
            self.terminated = true;
            return Poll::Ready(None);
        }

        // Drive any relay tasks that may have been added after processing `self.pairing_requests`.
        if !self.relay_tasks.is_terminated() {
            let _ = Pin::new(&mut self.relay_tasks).poll_next(cx);
        }

        // Drive any tasks removing unauthenticated bonds to completion.
        while let Poll::Ready(Some(())) = self.forget_tasks.poll_next_unpin(cx) {}

        // Check if any procedures have missed deadlines.
        self.poll_procedure_deadlines(cx);
        // Check if any pending pairing requests have missed deadlines.
        self.poll_pending_request_deadlines(cx);
        // Check if any retroactive pairing eligibility windows have expired.
        self.poll_retroactive_pairing_deadlines(cx);
        Poll::Pending
    }
}

impl FusedStream for PairingManager {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use async_utils::PollExt;
    use diagnostics_assertions::assert_data_tree;
    use fidl::client::QueryResponseFut;
    use fidl_fuchsia_bluetooth_sys::{
        AccessMarker, AccessRequestStream, PairingKeypress, PairingMarker, PairingRequestStream,
    };
    use fuchsia_bluetooth::types::Address;
    use fuchsia_inspect_derive::WithInspect;
    use futures::future::Either;
    use std::pin::pin;

    use crate::types::keys;

    /// Holds onto the necessary FIDL connections to facilitate pairing. Provides convenience
    /// methods to drive pairing procedures.
    pub struct MockPairing {
        pub pairing_svc: PairingProxy,
        pub downstream_pairing_server: PairingRequestStream,
        pub downstream_delegate_client: PairingDelegateProxy,
        pub upstream_delegate_server: PairingDelegateRequestStream,
        pub access_svc: AccessProxy,
        pub access_server: AccessRequestStream,
    }

    impl MockPairing {
        /// Builds and returns a PairingManager and MockPairing object which can be used to simulate
        /// upstream & downstream pairing actions.
        pub async fn new_with_manager() -> (PairingManager, Self) {
            let (pairing_svc, mut downstream_pairing_server) =
                fidl::endpoints::create_proxy_and_stream::<PairingMarker>();
            let (delegate, upstream_delegate_server) =
                fidl::endpoints::create_proxy_and_stream::<PairingDelegateMarker>();
            let (access_svc, access_server) =
                fidl::endpoints::create_proxy_and_stream::<AccessMarker>();
            let client = PairingArgs {
                input: InputCapability::None,
                output: OutputCapability::None,
                delegate,
            };
            let manager = PairingManager::new(
                pairing_svc.clone(),
                access_svc.clone(),
                client,
                MetricsLogger::default(),
            )
            .expect("can create pairing manager");
            // Expect the manager to register a delegate on behalf of the upstream.
            let (_, _, delegate, _) = downstream_pairing_server
                .select_next_some()
                .await
                .expect("fidl request")
                .into_set_pairing_delegate()
                .unwrap();
            let downstream_delegate_client = delegate.into_proxy();

            let mock = MockPairing {
                pairing_svc,
                downstream_pairing_server,
                downstream_delegate_client,
                upstream_delegate_server,
                access_svc,
                access_server,
            };
            (manager, mock)
        }

        /// Expects a `sys.Access/Forget` request for `id` and responds with `result`.
        pub async fn expect_forget_with_result(
            &mut self,
            id: PeerId,
            result: Result<(), SysError>,
        ) {
            let (received_id, responder) = self
                .access_server
                .select_next_some()
                .await
                .expect("fidl request")
                .into_forget()
                .expect("Access::Forget request");
            assert_eq!(received_id, id.into());
            let _ = responder.send(result);
        }

        /// Expects a `sys.Access/Forget` request for `id` and responds with success.
        pub async fn expect_forget(&mut self, id: PeerId) {
            self.expect_forget_with_result(id, Ok(())).await;
        }

        /// Expects that no `sys.Access` request has been made.
        pub fn expect_no_forget(&mut self) {
            assert_matches!(self.access_server.next().now_or_never(), None);
        }

        pub async fn expect_set_pairing_delegate(&mut self) {
            let (_, _, delegate, _) = self
                .downstream_pairing_server
                .select_next_some()
                .await
                .expect("fidl request")
                .into_set_pairing_delegate()
                .unwrap();
            self.downstream_delegate_client = delegate.into_proxy();
        }

        pub async fn expect_on_pairing_complete(&mut self, id: PeerId) {
            let (received_id, success, _) = self
                .upstream_delegate_server
                .select_next_some()
                .await
                .expect("fidl request")
                .into_on_pairing_complete()
                .unwrap();
            assert_eq!(received_id, id.into());
            assert!(success);
        }

        pub fn make_pairing_request(
            &self,
            id: PeerId,
            passkey: u32,
        ) -> QueryResponseFut<(bool, u32)> {
            self.make_pairing_request_internal(id, PairingMethod::PasskeyComparison, passkey)
        }

        pub fn make_pairing_request_with_address(
            &self,
            id: PeerId,
            address: Address,
            passkey: u32,
        ) -> QueryResponseFut<(bool, u32)> {
            let delegate = self.downstream_delegate_client.clone();
            delegate.on_pairing_request(
                &example_peer_with_address(id, address),
                PairingMethod::PasskeyComparison,
                passkey,
            )
        }

        /// Simulates a non-Fast Pair pairing request and completion sent to the downstream
        /// delegate while `PairingDelegateOwner::Upstream` is active.
        pub fn complete_upstream_pairing(&self, id: PeerId, address: Address, success: bool) {
            let _request_fut = self.make_pairing_request_with_address(id, address, 123456);
            self.downstream_delegate_client
                .on_pairing_complete(&id.into(), success)
                .expect("fidl request");
        }

        fn make_pairing_request_internal(
            &self,
            id: PeerId,
            method: PairingMethod,
            passkey: u32,
        ) -> QueryResponseFut<(bool, u32)> {
            let delegate = self.downstream_delegate_client.clone();
            delegate.on_pairing_request(&example_peer(id), method, passkey)
        }
    }

    #[fuchsia::test]
    async fn lifetime_tied_to_upstream() {
        let (mut manager, mock) = MockPairing::new_with_manager().await;
        assert!(!manager.is_terminated());

        // Upstream delegate server goes away - expect the manager to terminate.
        drop(mock.upstream_delegate_server);
        assert_matches!(manager.next().await, None);
        assert!(manager.is_terminated());

        // Discarding the manager should signal termination to the downstream.
        drop(manager);
        let _ = mock.downstream_delegate_client.on_closed().await.expect("gracefully closed");
    }

    #[fuchsia::test]
    async fn downstream_termination_closes_manager() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;
        assert!(!manager.is_terminated());

        // Downstream goes away - manager should finish.
        drop(mock.downstream_delegate_client);
        assert_matches!(manager.next().await, None);
        assert!(manager.is_terminated());

        // Discarding the manager should signal termination to the upstream.
        drop(manager);
        let upstream_event = mock.upstream_delegate_server.next().await;
        assert_matches!(upstream_event, None);
    }

    #[fuchsia::test]
    async fn pairing_relay() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // Manager stream should constantly be polled. We don't expect any events since it is in
        // relay mode.
        let _manager_task = fuchsia_async::Task::local(async move {
            let event = manager.next().await;
            panic!("Unexpected pairing manager event {:?}", event);
        });

        // OnPairingRequest propagated to upstream.
        let sent_passkey = 123456;
        let downstream_fut = pin!(mock.make_pairing_request(PeerId(123), sent_passkey));
        let upstream_fut = pin!(mock.upstream_delegate_server.select_next_some());
        match futures::future::select(downstream_fut, upstream_fut).await {
            Either::Left(_) => panic!("Pairing Request fut resolved before receiving response"),
            Either::Right((Ok(upstream_result), downstream_fut)) => {
                let (_peer, method, displayed_passkey, responder) =
                    upstream_result.into_on_pairing_request().unwrap();
                assert_eq!(method, PairingMethod::PasskeyComparison); // Used in `MockPairing`.
                assert_eq!(displayed_passkey, sent_passkey);
                // Respond positively.
                let _ = responder.send(true, sent_passkey);
                // Downstream request should resolve.
                let downstream_result = downstream_fut.await.expect("should resolve ok");
                assert_eq!(downstream_result, (true, sent_passkey));
            }
            Either::Right((r, _downstream_fut)) => panic!("Unexpected result: {:?}", r),
        }

        // OnPairingComplete propagated to upstream.
        mock.downstream_delegate_client
            .on_pairing_complete(&PeerId(123).into(), true)
            .expect("fidl request");
        let _event = mock
            .upstream_delegate_server
            .select_next_some()
            .await
            .expect("fidl request")
            .into_on_pairing_complete()
            .unwrap();

        // OnRemoteKeypress propagated to upstream.
        mock.downstream_delegate_client
            .on_remote_keypress(&PeerId(123).into(), PairingKeypress::DigitEntered)
            .expect("fidl request");
        let _event = mock
            .upstream_delegate_server
            .select_next_some()
            .await
            .expect("fidl request")
            .into_on_remote_keypress()
            .unwrap();
    }

    #[fuchsia::test]
    async fn remote_keypress_is_no_op() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // Fast Pair mode - requesting this should result in a new downstream request.
        assert_matches!(manager.claim_delegate(), Ok(_));
        mock.expect_set_pairing_delegate().await;

        // Keypress event is irrelevant in Fast Pair mode. No stream item.
        mock.downstream_delegate_client
            .on_remote_keypress(&PeerId(123).into(), PairingKeypress::DigitEntered)
            .expect("fidl request");
        assert_matches!(manager.next().now_or_never(), None);
    }

    const EXAMPLE_PEER_ADDRESS: Address = Address::Public([1, 2, 3, 4, 5, 6]);

    pub(crate) fn example_peer(id: PeerId) -> fidl_fuchsia_bluetooth_sys::Peer {
        example_peer_with_address(id, EXAMPLE_PEER_ADDRESS)
    }

    pub(crate) fn example_peer_with_address(
        id: PeerId,
        address: Address,
    ) -> fidl_fuchsia_bluetooth_sys::Peer {
        let peer = Peer {
            id,
            address,
            technology: fidl_fuchsia_bluetooth_sys::TechnologyType::DualMode,
            connected: false,
            bonded: false,
            name: None,
            appearance: None,
            device_class: None,
            rssi: None,
            tx_power: None,
            le_services: vec![],
            bredr_services: vec![],
        };
        (&peer).into()
    }

    #[fuchsia::test]
    async fn claim_delegate_with_pairing_events() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A new pairing procedure is started between us and the peer. Expect the delegate to be
        // claimed.
        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Peer tries to pair - no stream item since pairing isn't complete.
        let request_fut = mock.make_pairing_request(id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // Remote peer wants to compare passkeys - no stream item since pairing isn't complete.
        let passkey = manager.compare_passkey(id, 555666).expect("successful comparison");
        assert_eq!(passkey, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // Expect the pairing request to complete as passkeys have been verified.
        let result = request_fut.await.expect("fidl response");
        assert_eq!(result, (true, 555666));

        // Downstream server signals pairing completion.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), true)
            .expect("valid fidl request");
        // Expect a Pairing Manager stream item indicating completion of pairing with the peer.
        let result = manager.select_next_some().await;
        assert_eq!(result, id);
        // Classic/LE pairing is complete, so PairingManager hands pairing capabilities back to the
        // upstream while keeping the shared secret for subsequent GATT steps.
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // The peer will make a GATT Write to the Account Key characteristic. The shared secret will
        // still be available as the peer can optionally set a personalized name via GATT.
        assert_matches!(manager.account_key_write(id), Ok(_));
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // The peer can set the personalized name. This is the last step in the procedure.
        assert_matches!(manager.complete_pairing_procedure(id), Ok(_));
        // The shared secret should no longer be saved, and the delegate is already owned by
        // upstream.
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
        assert_matches!(manager.key_for_procedure(&id), None);
    }

    #[fuchsia::test]
    async fn invalid_fast_pair_method_is_rejected() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let methods = vec![
            PairingMethod::Consent,
            PairingMethod::PasskeyDisplay,
            PairingMethod::PasskeyEntry,
        ];
        for unsupported_pairing_method in methods {
            // A new pairing procedure is started between us and the peer.
            let id = PeerId(123);
            assert_matches!(
                manager.new_pairing_procedure(
                    id,
                    keys::tests::example_aes_key(),
                    PairingType::Initial
                ),
                Ok(_)
            );
            mock.expect_set_pairing_delegate().await;
            assert_matches!(manager.key_for_procedure(&id), Some(_));

            let request_fut =
                pin!(mock.make_pairing_request_internal(id, unsupported_pairing_method, 555666));
            let manager_fut = pin!(manager.select_next_some());

            match futures::future::select(manager_fut, request_fut).await {
                Either::Left(_) => panic!("Unexpected Pairing Manager stream item"),
                Either::Right((Ok((false, 0)), manager_fut)) => {
                    // Should be handled gracefully, and rejected. There are no other active pairing
                    // procedures so pairing is handed back to upstream.
                    let () = mock.expect_set_pairing_delegate().await;
                    // No pairing event.
                    assert_matches!(manager_fut.now_or_never(), None);
                }
                Either::Right((r, _mgr_fut)) => panic!("Unexpected result: {:?}", r),
            }
        }
    }

    #[fuchsia::test]
    async fn no_stream_item_when_pairing_is_unsuccessful() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A new pairing procedure is started between us and the peer.
        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Peer tries to pair - no stream item since pairing isn't complete.
        let request_fut = mock.make_pairing_request(id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // Remote peer requests to compare passkeys.
        let passkey = manager.compare_passkey(id, 555666).expect("successful comparison");
        assert_eq!(passkey, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // Expect the pairing request to complete as passkeys have been verified.
        let result = request_fut.await.expect("fidl response");
        assert_eq!(result, (true, 555666));

        // Peer rejects pairing via the downstream `sys.Pairing` server.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), false)
            .expect("valid fidl request");
        let manager_fut = pin!(manager.select_next_some());
        // There are no active pairing procedures so pairing is handed back to upstream.
        let expect_fut = pin!(mock.expect_set_pairing_delegate());
        match futures::future::select(manager_fut, expect_fut).await {
            Either::Left(_) => panic!("Unexpected Pairing Manager stream item"),
            Either::Right(((), _manager_fut)) => {}
        }
        // Per GFPS Pairing Procedure Step 16, if pairing fails, then the shared secret must be
        // discarded.
        assert_matches!(manager.key_for_procedure(&id), None);
        // Can't complete the procedure since pairing failed.
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));
    }

    #[fuchsia::test]
    async fn passkey_mismatch_with_single_request_returns_provider_passkey() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A new pairing procedure is started between us and the peer.
        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Peer tries to pair - no stream item since pairing isn't complete.
        let provider_passkey = 123456;
        let request_fut = mock.make_pairing_request(id, provider_passkey);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // Remote peer requests to compare passkeys (Seeker passkey is sent over GATT). The passkeys
        // don't match, but the only pending request is the Seeker's. The Provider passkey is
        // returned to be notified (GFPS Step 14). Because the Seeker didn't prove ownership of the
        // request, the request is rejected and the procedure is canceled.
        let seeker_passkey = 987654;
        assert_eq!(
            manager.compare_passkey(id, seeker_passkey).expect("comparison"),
            provider_passkey
        );
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_eq!(request_fut.await.expect("fidl response"), (false, 0));
        assert_matches!(manager.key_for_procedure(&id), None);
        // There are no active pairing procedures so pairing is handed back to upstream.
        mock.expect_set_pairing_delegate().await;
    }

    #[fuchsia::test]
    async fn passkey_mismatch_with_multiple_requests_returns_error() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Two pairing requests with different passkeys.
        let mut request_fut1 = pin!(mock.make_pairing_request(PeerId(456), 111111));
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let mut request_fut2 = pin!(mock.make_pairing_request(PeerId(789), 222222));
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The Seeker's passkey matches neither request, and the Seeker's request can't be
        // identified. The comparison fails and neither request is resolved.
        assert_matches!(manager.compare_passkey(le_id, 999999), Err(Error::Internal(_)));
        assert_matches!(request_fut1.as_mut().now_or_never(), None);
        assert_matches!(request_fut2.as_mut().now_or_never(), None);

        // The procedure is canceled by the owner of the PairingManager after the failed comparison.
        // No procedure can claim the pending requests, so both are rejected.
        assert_matches!(manager.cancel_pairing_procedure(&le_id), Ok(_));
        assert_eq!(request_fut1.await.expect("fidl response"), (false, 0));
        assert_eq!(request_fut2.await.expect("fidl response"), (false, 0));
        mock.expect_set_pairing_delegate().await;
    }

    #[fuchsia::test]
    async fn passkey_matching_multiple_requests_rejects_all() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Two pairing requests with the same passkey.
        let request_fut1 = mock.make_pairing_request(PeerId(456), 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let request_fut2 = mock.make_pairing_request(PeerId(789), 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The Seeker's request can't be identified, so both are rejected.
        assert_matches!(manager.compare_passkey(le_id, 555666), Err(Error::Internal(_)));
        assert_eq!(request_fut1.await.expect("fidl response"), (false, 0));
        assert_eq!(request_fut2.await.expect("fidl response"), (false, 0));
    }

    #[fuchsia::test]
    async fn passkey_mismatch_does_not_affect_other_procedures() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id1 = PeerId(1);
        let le_id2 = PeerId(2);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id1,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(
            manager.new_pairing_procedure(
                le_id2,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );

        // Both Seekers make pairing requests over BR/EDR.
        let mut request_fut1 = pin!(mock.make_pairing_request(PeerId(11), 111111));
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let mut request_fut2 = pin!(mock.make_pairing_request(PeerId(22), 222222));
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The first Seeker sends a passkey that doesn't match any pairing request. The comparison
        // fails and the owner of the PairingManager cancels the first procedure.
        assert_matches!(manager.compare_passkey(le_id1, 999999), Err(Error::Internal(_)));
        assert_matches!(manager.cancel_pairing_procedure(&le_id1), Ok(_));
        // The second procedure is still active, so the Pairing Delegate is not released and the
        // pending pairing requests are not rejected.
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
        assert_matches!(manager.key_for_procedure(&le_id2), Some(_));
        assert_matches!(request_fut1.as_mut().now_or_never(), None);
        assert_matches!(request_fut2.as_mut().now_or_never(), None);

        // The second Seeker's passkey is matched with its own pairing request.
        assert_matches!(manager.compare_passkey(le_id2, 222222), Ok(222222));
        assert_eq!(request_fut2.await.expect("fidl response"), (true, 222222));
        // No procedure can claim the remaining request, so it is rejected.
        assert_eq!(request_fut1.await.expect("fidl response"), (false, 0));
    }

    #[fuchsia::test]
    async fn duplicate_pairing_procedure_is_error() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Trying to start a new pairing procedure for the same peer is an Error.
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Err(Error::Internal(_))
        );
        // Don't expect a new delegate to be claimed.
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
    }

    #[fuchsia::test]
    async fn new_procedure_claim_delegate_error_does_not_insert_procedure() {
        let (mut manager, _mock) = MockPairing::new_with_manager().await;
        // One-way FIDL calls ignore `PEER_CLOSED`, so use a channel without `WRITE` rights to
        // make `set_pairing_delegate` fail when claiming the Pairing Delegate.
        let (client_ch, _server_ch) = zx::Channel::create();
        let read_only_ch = client_ch
            .replace_handle(zx::Rights::READ | zx::Rights::WAIT)
            .expect("can remove write right");
        manager.pairing = PairingProxy::from_channel(fasync::Channel::from_channel(read_only_ch));

        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Err(Error::Fidl(_))
        );
        assert_matches!(manager.key_for_procedure(&id), None);
    }

    #[fuchsia::test]
    async fn unexpected_procedure_updates_is_error() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;
        let id = PeerId(123);

        // No active procedure: all updates fail.
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.account_key_write(id), Err(Error::Internal(_)));
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));

        // State: `AwaitingPasskey`.
        // `account_key_write` and `complete_pairing_procedure` are invalid; `compare_passkey` also
        // fails until a pairing request is pending.
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(())
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.account_key_write(id), Err(Error::Internal(_)));
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));

        // State: `AwaitingPairingComplete` (after matching passkey).
        // All GATT procedure updates fail while waiting for `OnPairingComplete`.
        let request_fut = mock.make_pairing_request(id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_matches!(manager.compare_passkey(id, 555666), Ok(555666));
        assert_eq!(request_fut.await.expect("fidl response"), (true, 555666));
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.account_key_write(id), Err(Error::Internal(_)));
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));

        // State: `AwaitingAccountKey` (after `OnPairingComplete`).
        // Only `account_key_write` is valid next.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), true)
            .expect("valid fidl request");
        assert_eq!(manager.select_next_some().await, id);
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));

        // State: `AwaitingPersonalizedName` (after `account_key_write`).
        // Only `complete_pairing_procedure` is valid next.
        assert_matches!(manager.account_key_write(id), Ok(()));
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.account_key_write(id), Err(Error::Internal(_)));

        // State: `Completed` (after `complete_pairing_procedure`).
        // The procedure is removed and all further updates fail.
        assert_matches!(manager.complete_pairing_procedure(id), Ok(()));
        assert_matches!(manager.compare_passkey(id, 555666), Err(Error::Internal(_)));
        assert_matches!(manager.account_key_write(id), Err(Error::Internal(_)));
        assert_matches!(manager.complete_pairing_procedure(id), Err(Error::Internal(_)));
    }

    // TODO(https://fxbug.dev/42053956): This test will be obsolete if the PairingManager is resilient to
    // ordering of events.
    #[fuchsia::test]
    async fn complete_pairing_before_on_pairing_complete_is_error() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Peer tries to pair - no stream item since pairing isn't complete.
        let request_fut = mock.make_pairing_request(id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // No stream item after comparing passkeys since pairing isn't complete.
        let passkey = manager.compare_passkey(id, 555666).expect("successful comparison");
        assert_eq!(passkey, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // Expect the pairing request to resolve as passkeys have been verified.
        let result = request_fut.await.expect("fidl response");
        assert_eq!(result, (true, 555666));

        // Attempting to complete pairing before OnPairingComplete is an Error.
        assert_matches!(manager.complete_pairing_procedure(id), Err(_));
    }

    #[fuchsia::test]
    async fn concurrent_pairing_procedures() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id1 = PeerId(1);
        let le_id2 = PeerId(2);
        // We can start two pairing procedures - we only expect one SetPairingDelegate since Fast
        // Pair will request it after the first call.
        assert_matches!(
            manager.new_pairing_procedure(
                le_id1,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        assert_matches!(
            manager.new_pairing_procedure(
                le_id2,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);

        // Both Seekers make pairing requests over BR/EDR, in the opposite order in which they
        // started Fast Pair.
        let bredr_id1 = PeerId(11);
        let bredr_id2 = PeerId(22);
        let mut request_fut2 = pin!(mock.make_pairing_request(bredr_id2, 222222));
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let request_fut1 = mock.make_pairing_request(bredr_id1, 111111);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // Match first Seeker's procedure with its pairing request using PassKey.
        assert_matches!(manager.compare_passkey(le_id1, 111111), Ok(111111));
        assert_eq!(request_fut1.await.expect("fidl response"), (true, 111111));
        // The second Seeker's request should still be pending.
        assert_matches!(request_fut2.as_mut().now_or_never(), None);
        assert_matches!(manager.compare_passkey(le_id2, 222222), Ok(222222));
        assert_eq!(request_fut2.await.expect("fidl response"), (true, 222222));

        // Pairing completion is attributed to the correct LE PeerId.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&bredr_id2.into(), true)
            .expect("valid fidl request");
        assert_eq!(manager.select_next_some().await, le_id2);
        // Because `le_id1` is still awaiting pairing completion, the Pairing Delegate should not be
        // released yet.
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);

        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&bredr_id1.into(), true)
            .expect("valid fidl request");
        assert_eq!(manager.select_next_some().await, le_id1);
        // Both procedures have completed Classic/LE pairing, so the Pairing Delegate is released
        // back to the upstream client before Account Key writes occur.
        mock.expect_set_pairing_delegate().await;

        // First Seeker makes a GATT write to the Account Key characteristic and completes.
        assert_matches!(manager.account_key_write(le_id1), Ok(_));
        assert_matches!(manager.complete_pairing_procedure(le_id1), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
        assert_matches!(manager.key_for_procedure(&le_id1), None);
        assert_matches!(manager.key_for_procedure(&le_id2), Some(_));

        // Completing the second Seeker's procedure cleans up its state.
        assert_matches!(manager.account_key_write(le_id2), Ok(_));
        assert_matches!(manager.complete_pairing_procedure(le_id2), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
        assert_matches!(manager.key_for_procedure(&le_id2), None);
    }

    #[fuchsia::test]
    async fn pairing_request_from_unrelated_peer_does_not_hijack_procedure() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // The Seeker starts Fast Pair over LE.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // An unrelated peer makes a pairing request before the Seeker does.
        let attacker_id = PeerId(456);
        let attacker_passkey = 111111;
        let attacker_request_fut = mock.make_pairing_request(attacker_id, attacker_passkey);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The Seeker makes its pairing request over BR/EDR.
        let seeker_bredr_id = PeerId(789);
        let seeker_passkey = 222222;
        let seeker_request_fut = mock.make_pairing_request(seeker_bredr_id, seeker_passkey);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The Seeker's passkey (received over GATT) should be matched with the Seeker's pairing
        // request, not the unrelated peer's.
        let passkey =
            manager.compare_passkey(le_id, seeker_passkey).expect("successful comparison");
        assert_eq!(passkey, seeker_passkey);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let result = seeker_request_fut.await.expect("fidl response");
        assert_eq!(result, (true, seeker_passkey));
        // No other procedure can claim the unrelated peer's request, so it is rejected.
        let (accepted, _) = attacker_request_fut.await.expect("fidl response");
        assert!(!accepted);

        // Pairing completion over BR/EDR is attributed to the Seeker's LE PeerId, and the
        // PairingDelegate is immediately handed back to the upstream client.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&seeker_bredr_id.into(), true)
            .expect("valid fidl request");
        let result = manager.select_next_some().await;
        assert_eq!(result, le_id);
        mock.expect_set_pairing_delegate().await;

        assert_matches!(manager.account_key_write(le_id), Ok(_));
        assert_matches!(manager.complete_pairing_procedure(le_id), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
    }

    #[fuchsia::test]
    async fn pairing_request_without_procedure_awaiting_pairing_is_rejected() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A personalized name write doesn't involve pairing.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::PersonalizedNameWrite
            ),
            Ok(_)
        );
        // Fast Pair owns the delegate (e.g. on behalf of a procedure that has since finished).
        assert_matches!(manager.claim_delegate(), Ok(_));
        mock.expect_set_pairing_delegate().await;

        // A pairing request is immediately rejected since there is no procedure awaiting pairing
        // (i.e. a procedure in the `AwaitingPasskey` state) to claim it.
        let request_fut = mock.make_pairing_request(PeerId(456), 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_eq!(request_fut.await.expect("fidl response"), (false, 0));
        // The personalized name write procedure is unaffected.
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));
    }

    #[fuchsia::test]
    async fn pairing_request_from_same_peer_replaces_pending_request() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        let bredr_id = PeerId(789);
        let first_request_fut = mock.make_pairing_request(bredr_id, 111111);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        let second_request_fut = mock.make_pairing_request(bredr_id, 222222);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // The older request is rejected.
        assert_eq!(first_request_fut.await.expect("fidl response"), (false, 0));

        assert_matches!(manager.compare_passkey(le_id, 222222), Ok(222222));
        assert_eq!(second_request_fut.await.expect("fidl response"), (true, 222222));
    }

    #[fuchsia::test]
    async fn pairing_request_over_pending_limit_is_rejected() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        let mut request_futs = Vec::new();
        for i in 0..MAX_PENDING_PAIRING_REQUESTS {
            let i = i as u64;
            request_futs.push(mock.make_pairing_request(PeerId(1000 + i), 100000 + i as u32));
            assert_matches!(manager.select_next_some().now_or_never(), None);
        }

        // Pending requests are at capacity - the newest request is rejected.
        let extra_request_fut = mock.make_pairing_request(PeerId(2000), 999999);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_eq!(extra_request_fut.await.expect("fidl response"), (false, 0));

        // Requests that were already pending can still be matched.
        assert_matches!(manager.compare_passkey(le_id, 100000), Ok(100000));
        let mut request_futs = request_futs.into_iter();
        let first_request_fut = request_futs.next().unwrap();
        assert_eq!(first_request_fut.await.expect("fidl response"), (true, 100000));
        // The rest are rejected since no other procedure can claim them.
        for request_fut in request_futs {
            assert_eq!(request_fut.await.expect("fidl response"), (false, 0));
        }
    }

    #[fuchsia::test]
    fn pairing_request_pending_past_deadline_is_rejected() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let setup_fut = MockPairing::new_with_manager();
        let mut setup_fut = pin!(setup_fut);
        let (mut manager, mut mock) =
            exec.run_until_stalled(&mut setup_fut).expect("can create pairing manager");

        let le_id1 = PeerId(1);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id1,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        {
            let expect_fut = mock.expect_set_pairing_delegate();
            let mut expect_fut = pin!(expect_fut);
            let () = exec.run_until_stalled(&mut expect_fut).expect("pairing delegate request");
        }

        // An unrelated peer makes a pairing request.
        let first_request_fut = mock.make_pairing_request(PeerId(456), 111111);
        let mut first_request_fut = pin!(first_request_fut);
        let _ =
            exec.run_until_stalled(&mut first_request_fut).expect_pending("waiting for response");
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");

        // Halfway through the first deadline, a second Seeker starts a procedure and makes its
        // pairing request.
        let half_dur = Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION / 2;
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(!exec.wake_expired_timers());
        let le_id2 = PeerId(2);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id2,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        let second_request_fut = mock.make_pairing_request(PeerId(789), 222222);
        let mut second_request_fut = pin!(second_request_fut);
        let _ =
            exec.run_until_stalled(&mut second_request_fut).expect_pending("waiting for response");
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");

        // The first request is rejected once its deadline is reached, while the second Seeker's
        // procedure and request are still active.
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        let (accepted, _) = exec
            .run_until_stalled(&mut first_request_fut)
            .expect("pairing response")
            .expect("result is OK");
        assert!(!accepted);
        let _ =
            exec.run_until_stalled(&mut second_request_fut).expect_pending("waiting for response");
        assert_matches!(manager.key_for_procedure(&le_id1), None);
        assert_matches!(manager.key_for_procedure(&le_id2), Some(_));

        // The second Seeker's passkey is matched with its request.
        assert_matches!(manager.compare_passkey(le_id2, 222222), Ok(222222));
        let result = exec
            .run_until_stalled(&mut second_request_fut)
            .expect("pairing response")
            .expect("result is OK");
        assert_eq!(result, (true, 222222));
    }

    #[fuchsia::test]
    async fn pairing_request_from_bound_bredr_id_cancels_procedure() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        let bredr_id = PeerId(789);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // The Seeker's BR/EDR pairing request is matched and bound to the procedure.
        let request_fut = mock.make_pairing_request(bredr_id, 123456);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_matches!(manager.compare_passkey(le_id, 123456), Ok(123456));
        assert_eq!(request_fut.await.expect("fidl response"), (true, 123456));

        // Another pairing request from the bound BR/EDR PeerId is unexpected. The request is
        // rejected and the procedure is canceled.
        let second_request_fut = mock.make_pairing_request(bredr_id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_eq!(second_request_fut.await.expect("fidl response"), (false, 0));
        assert_matches!(manager.key_for_procedure(&le_id), None);
        // There are no other active procedures so the Pairing Delegate is handed back.
        mock.expect_set_pairing_delegate().await;
    }

    #[fuchsia::test]
    async fn claim_and_release_delegate() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        assert_matches!(manager.claim_delegate(), Ok(_));
        mock.expect_set_pairing_delegate().await;
        // Claiming the delegate again is a no-op.
        assert_matches!(manager.claim_delegate(), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);

        // Releasing delegate should result in a new PairingDelegate being set.
        assert_matches!(manager.release_delegate(), Ok(_));
        mock.expect_set_pairing_delegate().await;
        // Releasing the delegate again is a no-op.
        assert_matches!(manager.release_delegate(), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);

        // Pairing Requests are routed upstream.
        mock.downstream_delegate_client
            .on_pairing_complete(&PeerId(123).into(), true)
            .expect("fidl request");
        assert_matches!(manager.next().now_or_never(), None);
        let _event = mock
            .upstream_delegate_server
            .select_next_some()
            .await
            .expect("fidl request")
            .into_on_pairing_complete()
            .unwrap();
    }

    #[fuchsia::test]
    fn procedure_terminates_after_timer_completes() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let id = PeerId(123);
        let procedure = Procedure::new(id, keys::tests::example_aes_key(), PairingType::Initial);
        let mut procedure = pin!(procedure);
        let _ = exec.run_until_stalled(&mut procedure).expect_pending("deadline not reached");

        // Advancing time by less than the deadline means the Procedure Future isn't done.
        let half_dur = Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION / 2;
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(!exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut procedure).expect_pending("deadline not reached");

        // Transitioning the procedure should reset the timer.
        let _ = procedure.transition(ProcedureState::AwaitingPairingComplete);
        let _ = exec.run_until_stalled(&mut procedure).expect_pending("deadline not reached");

        // Even though the original deadline was met, the procedure should still be active since it
        // has a new deadline.
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(!exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut procedure).expect_pending("deadline not reached");

        // Deadline reached - procedure should resolve.
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(exec.wake_expired_timers());
        let finished_id = exec.run_until_stalled(&mut procedure).expect("deadline reached");
        assert_eq!(finished_id, id);
    }

    #[fuchsia::test]
    fn procedure_evicted_after_deadline() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let setup_fut = MockPairing::new_with_manager();
        let mut setup_fut = pin!(setup_fut);
        let (mut manager, mut mock) =
            exec.run_until_stalled(&mut setup_fut).expect("can create pairing manager");

        // New procedure is started - the shared secret should be available.
        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        {
            let expect_fut = mock.expect_set_pairing_delegate();
            let mut expect_fut = pin!(expect_fut);
            let () = exec.run_until_stalled(&mut expect_fut).expect("pairing delegate request");
        }
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Deadline not reached yet - procedure is still in progress.
        let half_dur = Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION / 2;
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(!exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Peer makes a pairing request - this is held as pending and does not reset the
        // procedure's deadline.
        let request_fut = mock.make_pairing_request(id, 555666);
        let mut request_fut = pin!(request_fut);
        let _ = exec.run_until_stalled(&mut request_fut).expect_pending("waiting for response");
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Advancing time to the original deadline should evict the procedure - shared secret no
        // longer available.
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), None);
        // Peer's pairing request should be rejected as the procedure was canceled.
        let (accepted, _) = exec
            .run_until_stalled(&mut request_fut)
            .expect("pairing response")
            .expect("result is OK");
        assert!(!accepted);
        // Because there are no other active procedures, the PairingDelegate should be handed back.
        let expect_fut = mock.expect_set_pairing_delegate();
        let mut expect_fut = pin!(expect_fut);
        let () = exec.run_until_stalled(&mut expect_fut).expect("pairing delegate request");
    }

    #[fuchsia::test]
    async fn pairing_procedure_with_differing_ids() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // Define a remote peer that has been assigned an LE and BR/EDR PeerId.
        let le_id = PeerId(123);
        let bredr_id = PeerId(789);

        // Pairing procedure is always initiated by the LE peer.
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        // Peer tries to pair over BR/EDR - no stream item since pairing isn't complete.
        let request_fut = mock.make_pairing_request(bredr_id, 123456);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // Procedure should still be active.
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));

        // Remote peer wants to compare passkeys - no stream item since pairing isn't complete.
        let passkey = manager.compare_passkey(le_id, 123456).expect("successful comparison");
        assert_eq!(passkey, 123456);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        // Expect the pairing request to complete as passkeys have been verified.
        let result = request_fut.await.expect("fidl response");
        assert_eq!(result, (true, 123456));

        // Downstream server signals pairing completion.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&bredr_id.into(), true)
            .expect("valid fidl request");
        // Expect a Pairing Manager stream item indicating completion of pairing with the peer. The
        // LE PeerId associated with this peer is used, and the PairingDelegate is handed back to
        // the upstream client while the shared secret remains available for GATT steps.
        let result = manager.select_next_some().await;
        assert_eq!(result, le_id);
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));

        // Expect an Account Key write.
        assert_matches!(manager.account_key_write(le_id), Ok(_));

        // PairingManager owner will complete pairing once the peer sets a personalized name.
        assert_matches!(manager.complete_pairing_procedure(le_id), Ok(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
        assert_matches!(manager.key_for_procedure(&le_id), None);
    }

    /// This test exercises the eviction flow in the event the peer does not set a personalized name
    /// We expect the procedure to be active for `Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION`
    /// after the Account Key write.
    #[fuchsia::test]
    fn procedure_without_name_write_evicted_after_deadline() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let setup_fut = MockPairing::new_with_manager();
        let mut setup_fut = pin!(setup_fut);
        let (mut manager, mut mock) =
            exec.run_until_stalled(&mut setup_fut).expect("can create pairing manager");

        // New procedure is started.
        let id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        {
            let expect_fut = mock.expect_set_pairing_delegate();
            let mut expect_fut = pin!(expect_fut);
            let () = exec.run_until_stalled(&mut expect_fut).expect("pairing delegate request");
        }

        // Deadline not reached yet - procedure is still in progress.
        let half_dur = Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION / 2;
        exec.set_fake_time(fasync::MonotonicInstant::after(half_dur));
        assert!(!exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Peer tries to pair.
        let request_fut = mock.make_pairing_request(id, 555666);
        let mut request_fut = pin!(request_fut);
        let _ = exec.run_until_stalled(&mut request_fut).expect_pending("waiting for response");
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), Some(_));

        // Passkey comparison - pairing request should resolve.
        let passkey = manager.compare_passkey(id, 555666).expect("successful comparison");
        assert_eq!(passkey, 555666);
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        let _ = exec.run_until_stalled(&mut request_fut).expect("pairing response");

        // Downstream server signals pairing completion - expect the stream item with `id`.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), true)
            .expect("valid fidl request");
        let finished_id = exec.run_until_stalled(&mut manager.next()).expect("stream item");
        assert_eq!(finished_id, Some(id));
        // Because Classic/LE pairing is complete and no other procedures await pairing, the
        // PairingDelegate should be handed back immediately.
        {
            let expect_fut = mock.expect_set_pairing_delegate();
            let mut expect_fut = pin!(expect_fut);
            let () = exec.run_until_stalled(&mut expect_fut).expect("pairing delegate request");
        }

        // Peer makes an Account Key write. This is the last mandatory step of the procedure.
        let _ = manager.account_key_write(id).expect("correct state");
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");

        // The peer doesn't optionally set the device name. After the deadline, the procedure should
        // be completed and cleaned up.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            Procedure::DEFAULT_PROCEDURE_TIMEOUT_DURATION,
        ));
        assert!(exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");
        assert_matches!(manager.key_for_procedure(&id), None);
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
    }

    #[fuchsia::test]
    async fn personalized_name_write_doesnt_claim_delegate() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::PersonalizedNameWrite
            ),
            Ok(_)
        );
        // We don't expect the pairing delegate to be claimed since pairing is not needed.
        assert_matches!(mock.expect_set_pairing_delegate().now_or_never(), None);
        // Shared secret should be accessible.
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));

        // No need for passkey comparison or Account Key write.
        assert_matches!(manager.compare_passkey(le_id, 123456), Err(_));
        assert_matches!(manager.account_key_write(le_id), Err(_));
        // No pairing steps so the procedure can be completed whenever.
        assert_matches!(manager.complete_pairing_procedure(le_id), Ok(_));
        assert_matches!(manager.key_for_procedure(&le_id), None);
    }

    #[fuchsia::test]
    async fn retroactive_pairing_procedure() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // First, the peer completes non-Fast Pair pairing via the upstream delegate.
        let bredr_id = PeerId(456);
        let seeker_address = EXAMPLE_PEER_ADDRESS;
        mock.complete_upstream_pairing(bredr_id, seeker_address, /* success= */ true);
        assert_matches!(manager.next().now_or_never(), None);

        // The peer then initiates a retroactive pairing procedure over LE (potentially with a
        // different PeerId).
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address }
            ),
            Ok(_)
        );
        // We don't expect the pairing delegate to be claimed since pairing is already complete.
        assert_matches!(mock.expect_set_pairing_delegate().now_or_never(), None);
        // Shared secret should be accessible.
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));

        // No need for passkey comparison.
        assert_matches!(manager.compare_passkey(le_id, 123456), Err(_));
        // When the peer writes an Account Key, the retroactive pairing flow is complete.
        assert_matches!(manager.account_key_write(le_id), Ok(_));
        assert_matches!(manager.complete_pairing_procedure(le_id), Ok(_));
        assert_matches!(manager.key_for_procedure(&le_id), None);

        // A second retroactive pairing procedure for the same bond is rejected because only one
        // Account Key write is permitted per non-Fast Pair bond.
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address }
            ),
            Err(_)
        );
    }

    #[fuchsia::test]
    async fn retroactive_pairing_without_upstream_bond_is_rejected() {
        let (mut manager, mock) = MockPairing::new_with_manager().await;

        // Starting a retroactive procedure without any prior upstream pairing fails.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address: EXAMPLE_PEER_ADDRESS }
            ),
            Err(_)
        );
        assert_matches!(manager.key_for_procedure(&le_id), None);

        // If upstream pairing fails (`success = false`), retroactive pairing is also rejected.
        let bredr_id = PeerId(456);
        mock.complete_upstream_pairing(bredr_id, EXAMPLE_PEER_ADDRESS, /* success= */ false);
        assert_matches!(manager.next().now_or_never(), None);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address: EXAMPLE_PEER_ADDRESS }
            ),
            Err(_)
        );
        assert_matches!(manager.key_for_procedure(&le_id), None);
    }

    #[fuchsia::test]
    fn retroactive_pairing_after_timeout_is_rejected() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(1_000_000_000));

        let mut setup_fut = pin!(MockPairing::new_with_manager());
        let (mut manager, mock) =
            exec.run_until_stalled(&mut setup_fut).expect("can create pairing manager");

        // Peer completes non-Fast Pair pairing via the upstream delegate.
        let bredr_id = PeerId(456);
        let seeker_address = EXAMPLE_PEER_ADDRESS;
        mock.complete_upstream_pairing(bredr_id, seeker_address, /* success= */ true);
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");

        // Advancing time past the 60-second retroactive write window expires eligibility.
        exec.set_fake_time(fasync::MonotonicInstant::after(
            PairingManager::DEFAULT_RETROACTIVE_WRITE_TIMEOUT_DURATION,
        ));
        assert!(exec.wake_expired_timers());
        let _ = exec.run_until_stalled(&mut manager.next()).expect_pending("no stream item");

        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address }
            ),
            Err(_)
        );
        assert_matches!(manager.key_for_procedure(&le_id), None);
    }

    #[fuchsia::test]
    async fn pairing_success_without_passkey_match_forgets_bond() {
        for pairing_type in [PairingType::Initial, PairingType::Subsequent] {
            let (mut manager, mut mock) = MockPairing::new_with_manager().await;

            let id = PeerId(123);
            assert_matches!(
                manager.new_pairing_procedure(id, keys::tests::example_aes_key(), pairing_type),
                Ok(_)
            );
            mock.expect_set_pairing_delegate().await;

            // The system reports pairing success even though the Seeker's passkey was never
            // matched.
            let _ = mock
                .downstream_delegate_client
                .on_pairing_complete(&id.into(), true)
                .expect("valid fidl request");
            assert_matches!(manager.select_next_some().now_or_never(), None);

            // The procedure is canceled and the Pairing Delegate is handed back to the upstream.
            assert_matches!(manager.key_for_procedure(&id), None);
            mock.expect_set_pairing_delegate().await;
            // The unauthenticated bond is removed.
            mock.expect_forget(id).await;
        }
    }

    #[fuchsia::test]
    async fn bredr_pairing_success_without_passkey_match_forgets_bond() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A Seeker initiates Fast Pair over LE.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // The Seeker (or another peer) makes a pairing request over BR/EDR with a different
        // PeerId, which is held in `pending_requests`.
        let bredr_id = PeerId(456);
        let request_fut = mock.make_pairing_request(bredr_id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The system reports pairing success for `bredr_id` before any passkey comparison links it
        // to a procedure.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&bredr_id.into(), true)
            .expect("valid fidl request");
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The unanswered pairing request is rejected and the unauthenticated bond is removed.
        assert_eq!(request_fut.await.expect("fidl response"), (false, 0));
        mock.expect_forget(bredr_id).await;
    }

    #[fuchsia::test]
    async fn pending_request_failure_is_not_relayed_upstream() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // A Seeker initiates Fast Pair over LE.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // A peer's pairing request is intercepted by Fast Pair and held in `pending_requests`.
        let bredr_id = PeerId(456);
        let request_fut = mock.make_pairing_request(bredr_id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The pairing attempt fails (e.g. timeout or disconnect) before the Seeker sends its
        // passkey over GATT.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&bredr_id.into(), false)
            .expect("valid fidl request");
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The held request is rejected.
        // This should not be relayed to the upstream.
        assert_eq!(request_fut.await.expect("fidl response"), (false, 0));
        assert_matches!(mock.upstream_delegate_server.next().now_or_never(), None);
        mock.expect_no_forget();
        assert_matches!(manager.key_for_procedure(&le_id), Some(_));
    }

    #[fuchsia::test]
    async fn personalized_name_write_pairing_success_does_not_forget_bond() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // An Initial procedure with another peer gives Fast Pair ownership of the Pairing Delegate.
        let other_id = PeerId(456);
        assert_matches!(
            manager.new_pairing_procedure(
                other_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Personalized name write procedures are for already-bonded peers and start in
        // `AwaitingPersonalizedName`.
        let le_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::PersonalizedNameWrite
            ),
            Ok(_)
        );

        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&le_id.into(), true)
            .expect("valid fidl request");
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The existing bond is not removed. The unexpected event cancels the procedure.
        mock.expect_no_forget();
        assert_matches!(manager.key_for_procedure(&le_id), None);
        // The other procedure is unaffected.
        assert_matches!(manager.key_for_procedure(&other_id), Some(_));
    }

    #[fuchsia::test]
    async fn forget_error_is_not_fatal() {
        for result in [Err(SysError::Failed), Err(SysError::PeerNotFound)] {
            let (mut manager, mut mock) = MockPairing::new_with_manager().await;

            let id = PeerId(123);
            assert_matches!(
                manager.new_pairing_procedure(
                    id,
                    keys::tests::example_aes_key(),
                    PairingType::Initial
                ),
                Ok(_)
            );
            mock.expect_set_pairing_delegate().await;

            let _ = mock
                .downstream_delegate_client
                .on_pairing_complete(&id.into(), true)
                .expect("valid fidl request");
            assert_matches!(manager.select_next_some().now_or_never(), None);
            mock.expect_set_pairing_delegate().await;
            mock.expect_forget_with_result(id, result).await;

            // The error is handled gracefully and the PairingManager is still usable.
            assert_matches!(manager.select_next_some().now_or_never(), None);
            assert!(!manager.is_terminated());
            assert_matches!(
                manager.new_pairing_procedure(
                    id,
                    keys::tests::example_aes_key(),
                    PairingType::Initial
                ),
                Ok(_)
            );
            mock.expect_set_pairing_delegate().await;
        }
    }

    #[fuchsia::test]
    async fn retroactive_pairing_success_does_not_forget_bond() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // First, the peer completes non-Fast Pair pairing via the upstream delegate.
        let le_id = PeerId(123);
        let seeker_address = EXAMPLE_PEER_ADDRESS;
        mock.complete_upstream_pairing(le_id, seeker_address, /* success= */ true);
        assert_matches!(manager.next().now_or_never(), None);

        // An Initial procedure with another peer gives Fast Pair ownership of the Pairing Delegate.
        let other_id = PeerId(456);
        assert_matches!(
            manager.new_pairing_procedure(
                other_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Retroactive procedures start after Classic/LE pairing has already completed.
        assert_matches!(
            manager.new_pairing_procedure(
                le_id,
                keys::tests::example_aes_key(),
                PairingType::Retroactive { seeker_address }
            ),
            Ok(_)
        );

        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&le_id.into(), true)
            .expect("valid fidl request");
        assert_matches!(manager.select_next_some().now_or_never(), None);

        // The existing bond is not removed. The unexpected event cancels the procedure.
        mock.expect_no_forget();
        assert_matches!(manager.key_for_procedure(&le_id), None);
        // The other procedure is unaffected.
        assert_matches!(manager.key_for_procedure(&other_id), Some(_));
    }

    #[fuchsia::test]
    async fn pairing_complete_for_non_fast_pair_peer_is_relayed_upstream() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // An Initial procedure gives Fast Pair ownership of the Pairing Delegate.
        let fp_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                fp_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // `bt-host` reports completion for a peer Fast Pair never handled (e.g. a Classic pairing
        // that the upstream client accepted before Fast Pair claimed the delegate).
        for (classic_id, success) in [(PeerId(456), true), (PeerId(789), false)] {
            mock.downstream_delegate_client
                .on_pairing_complete(&classic_id.into(), success)
                .expect("valid fidl request");
            assert_matches!(manager.select_next_some().now_or_never(), None);

            // The outcome is relayed to the upstream client.
            let (received_id, received_success, _) = mock
                .upstream_delegate_server
                .select_next_some()
                .now_or_never()
                .expect("relayed to upstream")
                .expect("fidl request")
                .into_on_pairing_complete()
                .expect("OnPairingComplete");
            assert_eq!(received_id, classic_id.into());
            assert_eq!(received_success, success);
        }

        // The bond is not removed and the Fast Pair procedure is unaffected.
        mock.expect_no_forget();
        assert_matches!(manager.key_for_procedure(&fp_id), Some(_));
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);
    }

    #[fuchsia::test]
    async fn duplicate_pairing_success_does_not_forget_bond() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        let id = PeerId(123);
        let other_id = PeerId(456);
        assert_matches!(
            manager.new_pairing_procedure(id, keys::tests::example_aes_key(), PairingType::Initial),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;
        // Another active Initial procedure keeps Fast Pair in ownership of the Pairing Delegate
        // after `id` completes Classic/LE pairing.
        assert_matches!(
            manager.new_pairing_procedure(
                other_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );

        // Pairing completes for `id` after the Seeker's passkey is matched.
        let request_fut = mock.make_pairing_request(id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_matches!(manager.compare_passkey(id, 555666), Ok(_));
        assert_eq!(request_fut.await.expect("fidl response"), (true, 555666));
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), true)
            .expect("valid fidl request");
        assert_eq!(manager.select_next_some().await, id);

        // A second pairing success for the authenticated peer doesn't remove the bond.
        let _ = mock
            .downstream_delegate_client
            .on_pairing_complete(&id.into(), true)
            .expect("valid fidl request");
        assert_matches!(manager.select_next_some().now_or_never(), None);
        mock.expect_no_forget();
        // The unexpected event cancels `id`'s procedure while leaving `other_id` active.
        assert_matches!(manager.key_for_procedure(&id), None);
        assert_matches!(manager.key_for_procedure(&other_id), Some(_));
        assert_matches!(manager.cancel_pairing_procedure(&other_id), Ok(_));
        mock.expect_set_pairing_delegate().await;
    }

    #[fuchsia::test]
    async fn delegate_released_after_pairing_complete_allows_upstream_pairing() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // Peer 1 starts Fast Pair and completes Classic/LE pairing.
        let fp_id = PeerId(123);
        assert_matches!(
            manager.new_pairing_procedure(
                fp_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        let request_fut = mock.make_pairing_request(fp_id, 555666);
        assert_matches!(manager.select_next_some().now_or_never(), None);
        assert_matches!(manager.compare_passkey(fp_id, 555666), Ok(555666));
        assert_eq!(request_fut.await.expect("fidl response"), (true, 555666));

        mock.downstream_delegate_client
            .on_pairing_complete(&fp_id.into(), true)
            .expect("valid fidl request");
        assert_eq!(manager.select_next_some().await, fp_id);
        // The delegate is immediately released back to upstream while `fp_id` is in
        // `AwaitingAccountKey`.
        mock.expect_set_pairing_delegate().await;

        // While `fp_id` is still in `AwaitingAccountKey`, a non-Fast Pair peer's pairing request
        // is relayed to the upstream delegate instead of being rejected.
        let classic_id = PeerId(456);
        let _downstream_fut = mock.make_pairing_request(classic_id, 111222);
        assert_matches!(manager.next().now_or_never(), None);
        let upstream_req = mock
            .upstream_delegate_server
            .select_next_some()
            .now_or_never()
            .expect("relayed to upstream")
            .expect("fidl request");
        let (_peer, _method, passkey, _responder) = upstream_req.into_on_pairing_request().unwrap();
        assert_eq!(passkey, 111222);

        // `fp_id` can still write its Account Key and Personalized Name.
        assert_matches!(manager.account_key_write(fp_id), Ok(_));
        assert_matches!(manager.complete_pairing_procedure(fp_id), Ok(_));
    }

    #[fuchsia::test]
    async fn delegate_released_when_only_non_pairing_procedures_remain() {
        let (mut manager, mut mock) = MockPairing::new_with_manager().await;

        // Start a `PersonalizedNameWrite` procedure (which does not use the PairingDelegate)
        // alongside an `Initial` procedure (which claims the PairingDelegate).
        let name_id = PeerId(1);
        let initial_id = PeerId(2);
        assert_matches!(
            manager.new_pairing_procedure(
                name_id,
                keys::tests::example_aes_key(),
                PairingType::PersonalizedNameWrite
            ),
            Ok(_)
        );
        assert_matches!(mock.downstream_pairing_server.next().now_or_never(), None);

        assert_matches!(
            manager.new_pairing_procedure(
                initial_id,
                keys::tests::example_aes_key(),
                PairingType::Initial
            ),
            Ok(_)
        );
        mock.expect_set_pairing_delegate().await;

        // Canceling the `Initial` procedure immediately releases the PairingDelegate even though
        // the `PersonalizedNameWrite` procedure is still active.
        assert_matches!(manager.cancel_pairing_procedure(&initial_id), Ok(_));
        mock.expect_set_pairing_delegate().await;
        assert_matches!(manager.key_for_procedure(&name_id), Some(_));
    }

    #[test]
    fn pairing_manager_inspect_tree() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(5_000_000_000));
        let inspect = inspect::Inspector::default();
        let mut pairing_inspect =
            PairingManagerInspect::default().with_inspect(inspect.root(), "pairing").unwrap();

        // Default tree
        assert_data_tree!(@executor exec, inspect, root: {
            pairing: {
                owner: "",
                finished_procedures: {}
            }
        });

        pairing_inspect.set_owner(&PairingDelegateOwner::Upstream);
        let id = PeerId(123);
        let example_procedure_node = pairing_inspect.record_new_procedure(id, "procedure_1");
        example_procedure_node.bredr_id.set("123");
        example_procedure_node.state.set("AwaitingPersonalizedName");
        assert_data_tree!(@executor exec, inspect, root: {
            pairing: {
                owner: "Upstream",
                finished_procedures: {},
                procedure_1: {
                    le_id: "000000000000007b",
                    bredr_id: "123",
                    state: "AwaitingPersonalizedName",
                }
            }
        });

        // Procedure finishes some time later.
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));
        example_procedure_node.state.set("Completed");
        pairing_inspect.record_finished_procedure(id, fasync::MonotonicInstant::now());
        assert_data_tree!(@executor exec, inspect, root: {
            pairing: {
                owner: "Upstream",
                finished_procedures: {
                    "0": {
                        procedure_1: {
                            le_id: "000000000000007b",
                            bredr_id: "123",
                            state: "Completed",
                            pairing_time_seconds: 5u64,
                        }
                    }
                }
            }
        });

        pairing_inspect.set_owner(&PairingDelegateOwner::FastPair);
        assert_data_tree!(@executor exec, inspect, root: {
            pairing: {
                owner: "FastPair",
                finished_procedures: {
                    "0": {
                        procedure_1: {
                            le_id: "000000000000007b",
                            bredr_id: "123",
                            state: "Completed",
                            pairing_time_seconds: 5u64,
                        }
                    }
                }
            }
        });
    }
}
