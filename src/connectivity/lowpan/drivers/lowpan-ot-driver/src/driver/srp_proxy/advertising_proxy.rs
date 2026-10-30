// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use fidl::endpoints::{Proxy, create_endpoints};
use fidl_fuchsia_net_mdns::*;
use fuchsia_async as fasync;
use fuchsia_async::Task;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::prelude::*;
use futures::stream::StreamExt;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::Arc;
use std::time::Duration;

const MAX_PUBLISH_RETRIES: usize = 3;
const UNPUBLISH_SYNC_TIMEOUT: Duration = Duration::from_millis(250);

#[allow(clippy::collection_is_never_read)]
async fn wait_for_service_removal(
    service_type: String,
    instance_name: String,
    mut service_to_drop: Option<AdvertisingProxyServiceState>,
) -> Result<(), anyhow::Error> {
    let subscriber = connect_to_protocol::<ServiceSubscriber2Marker>()?;
    let (client, server) = create_endpoints::<ServiceSubscriptionListenerMarker>();

    subscriber.subscribe_to_service(
        &service_type,
        &ServiceSubscriptionOptions {
            exclude_local: Some(false),
            exclude_local_proxies: Some(false),
            ..Default::default()
        },
        client,
    )?;

    let mut stream = server.into_stream();

    while let Some(request) = stream.try_next().await? {
        match request {
            ServiceSubscriptionListenerRequest::OnInstanceLost { instance, responder, .. } => {
                let _ = responder.send();
                if instance == instance_name {
                    return Ok(());
                }
            }
            ServiceSubscriptionListenerRequest::OnInstanceDiscovered {
                instance,
                responder,
                ..
            } => {
                let _ = responder.send();
                if instance.instance.as_deref() == Some(instance_name.as_str()) {
                    service_to_drop.take();
                }
            }
            ServiceSubscriptionListenerRequest::OnInstanceChanged {
                instance, responder, ..
            } => {
                let _ = responder.send();
                if instance.instance.as_deref() == Some(instance_name.as_str()) {
                    service_to_drop.take();
                }
            }
            ServiceSubscriptionListenerRequest::OnQuery { responder, .. } => {
                let _ = responder.send();
            }
        }
    }
    bail!("Service subscription stream ended before removal of {:?}", instance_name)
}

#[allow(clippy::collection_is_never_read)]
async fn wait_for_host_removal(
    host_name: String,
    mut host_to_drop: Option<AdvertisingProxyHost>,
) -> Result<(), anyhow::Error> {
    let subscriber = connect_to_protocol::<HostNameSubscriberMarker>()?;
    let (client, server) = create_endpoints::<HostNameSubscriptionListenerMarker>();

    subscriber.subscribe_to_host_name(
        &host_name,
        &HostNameSubscriptionOptions {
            exclude_local: Some(false),
            exclude_local_proxies: Some(false),
            ..Default::default()
        },
        client,
    )?;

    let mut stream = server.into_stream();

    while let Some(request) = stream.try_next().await? {
        match request {
            HostNameSubscriptionListenerRequest::OnAddressesChanged { addresses, responder } => {
                let _ = responder.send();
                if addresses.is_empty() {
                    return Ok(());
                } else {
                    host_to_drop.take();
                }
            }
        }
    }
    bail!("Host subscription stream ended before removal of {:?}", host_name)
}

/// The advertising proxy handles taking hosts and services registered with the SRP server
/// and republishing them via local mDNS.
#[derive(Debug)]
pub struct AdvertisingProxy {
    inner: Arc<Mutex<AdvertisingProxyInner>>,
    mdns_result_receiver: Mutex<Option<mpsc::UnboundedReceiver<MdnsResultMessage>>>,
}

impl Drop for AdvertisingProxy {
    fn drop(&mut self) {
        // Make sure all advertised hosts get cleaned up.
        self.inner.lock().hosts.clear();
    }
}

#[derive(Debug)]
struct OutstandingUpdate {
    #[allow(dead_code)] // Used as HashMap key
    update_id: ot::SrpServerServiceUpdateId,
    host_name: CString,
    callback_count: u32,
}

#[derive(Debug)]
pub(crate) struct MdnsResultMessage {
    update_id: ot::SrpServerServiceUpdateId,
    result: Result<(), ot::Error>,
}

#[derive(Debug)]
pub struct UpdateTracker {
    sender: mpsc::UnboundedSender<MdnsResultMessage>,
    update_id: Option<ot::SrpServerServiceUpdateId>,
}

impl UpdateTracker {
    fn new(
        sender: mpsc::UnboundedSender<MdnsResultMessage>,
        update_id: Option<ot::SrpServerServiceUpdateId>,
    ) -> Self {
        Self { sender, update_id }
    }

    fn resolve(mut self, result: Result<(), ot::Error>) {
        if let Some(update_id) = self.update_id.take() {
            let _ = self.sender.unbounded_send(MdnsResultMessage { update_id, result });
        }
    }
}

impl Drop for UpdateTracker {
    fn drop(&mut self) {
        if let Some(update_id) = self.update_id.take() {
            let _ = self
                .sender
                .unbounded_send(MdnsResultMessage { update_id, result: Err(ot::Error::Failed) });
        }
    }
}

#[derive(Debug)]
struct AdvertisingProxyInner {
    srp_domain: String,
    hosts: HashMap<CString, AdvertisingProxyHost>,
    mdns_proxy_host_publisher: ProxyHostPublisherProxy,
    outstanding_updates: HashMap<ot::SrpServerServiceUpdateId, OutstandingUpdate>,
    mdns_result_sender: mpsc::UnboundedSender<MdnsResultMessage>,
}

#[derive(Debug)]
pub struct AdvertisingProxyHost {
    services: HashMap<CString, AdvertisingProxyServiceState>,
    service_publisher: ServiceInstancePublisherProxy,
    addresses: Vec<std::net::Ipv6Addr>,
    publisher_notifier: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AdvertisingProxyServiceInfo {
    name: CString,
    txt: Vec<Vec<u8>>,
    port: u16,
    priority: u16,
    weight: u16,
    subtypes: HashSet<String>,
}

impl AdvertisingProxyServiceInfo {
    fn new(srp_service: &ot::SrpServerService) -> Self {
        AdvertisingProxyServiceInfo {
            name: srp_service.instance_name_cstr().to_owned(),
            txt: srp_service.txt_entries().map(|x| x.unwrap().to_vec()).collect::<Vec<_>>(),
            port: srp_service.port(),
            priority: srp_service.priority(),
            weight: srp_service.weight(),
            subtypes: HashSet::new(),
        }
    }

    fn is_up_to_date(&self, srp_service: &ot::SrpServerService) -> bool {
        !srp_service.is_deleted() && self == &AdvertisingProxyServiceInfo::new(srp_service)
    }

    fn has_subtype(&self, subtype: &str) -> bool {
        self.subtypes.contains(subtype)
    }

    fn set_subtypes(&mut self, subtypes: Vec<String>) {
        self.subtypes.clear();
        for subtype in subtypes {
            self.subtypes.insert(subtype);
        }
    }

    fn into_service_instance_publication(self) -> ServiceInstancePublication {
        ServiceInstancePublication {
            port: Some(self.port),
            text: Some(self.txt),
            srv_priority: Some(self.priority),
            srv_weight: Some(self.weight),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub struct AdvertisingProxyService {
    info: Arc<Mutex<AdvertisingProxyServiceInfo>>,

    control_handle: ServiceInstancePublicationResponder_ControlHandle,

    #[allow(dead_code)]
    task: Task<Result>,
}

/// Represents the state of an advertised service instance.
///
/// When a service is first sent to mDNS for publication, it enters the `Publishing`
/// state which holds its `fuchsia_async::Task`. If the service is deleted by the
/// client before the mDNS FIDL call completes, this task is simply dropped from the
/// map and automatically aborted. Once publication completes successfully, it
/// transitions to the `Active` state.
#[derive(Debug)]
pub enum AdvertisingProxyServiceState {
    #[allow(dead_code)]
    Publishing {
        task: Task<()>,
        tracker: Arc<Mutex<Option<UpdateTracker>>>,
    },
    Active(AdvertisingProxyService),
}

impl AdvertisingProxyServiceState {
    fn is_up_to_date(&self, srp_service: &ot::SrpServerService) -> bool {
        match self {
            AdvertisingProxyServiceState::Active(svc) => svc.is_up_to_date(srp_service),
            AdvertisingProxyServiceState::Publishing { .. } => false,
        }
    }

    fn update(&self, info: AdvertisingProxyServiceInfo) -> Result<(), anyhow::Error> {
        match self {
            AdvertisingProxyServiceState::Active(svc) => svc.update(info),
            AdvertisingProxyServiceState::Publishing { .. } => {
                Err(anyhow::anyhow!("Service is still publishing"))
            }
        }
    }

    fn set_subtypes(&self, subtypes: Vec<String>) -> Result<(), anyhow::Error> {
        match self {
            AdvertisingProxyServiceState::Active(svc) => svc.set_subtypes(subtypes),
            AdvertisingProxyServiceState::Publishing { .. } => {
                Err(anyhow::anyhow!("Service is still publishing"))
            }
        }
    }
}

impl AdvertisingProxyService {
    fn is_up_to_date(&self, srp_service: &ot::SrpServerService) -> bool {
        self.info.lock().is_up_to_date(srp_service)
    }

    fn update(&self, info: AdvertisingProxyServiceInfo) -> Result<(), anyhow::Error> {
        *self.info.lock() = info;
        self.reannounce()
    }

    fn set_subtypes(&self, subtypes: Vec<String>) -> Result<(), anyhow::Error> {
        {
            let mut info = self.info.lock();
            info.set_subtypes(subtypes.clone());
        }
        self.control_handle.send_set_subtypes(&subtypes).map_err(Into::into)
    }

    fn reannounce(&self) -> Result<(), anyhow::Error> {
        Ok(self.control_handle.send_reannounce()?)
    }
}

impl AdvertisingProxy {
    pub fn new(instance: &ot::Instance) -> Result<AdvertisingProxy, anyhow::Error> {
        let (mdns_result_sender, mdns_result_receiver) = mpsc::unbounded::<MdnsResultMessage>();
        let inner = Arc::new(Mutex::new(AdvertisingProxyInner {
            srp_domain: instance.srp_server_get_domain().to_str()?.to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher: connect_to_protocol::<ProxyHostPublisherMarker>()?,
            outstanding_updates: HashMap::new(),
            mdns_result_sender,
        }));
        let ret = AdvertisingProxy {
            inner: inner.clone(),
            mdns_result_receiver: Mutex::new(Some(mdns_result_receiver)),
        };

        ret.inner.lock().publish_srp_all(inner.clone(), instance)?;

        instance.srp_server_set_service_update_fn(Some(
            move |ot_instance: &ot::Instance,
                  update_id: ot::SrpServerServiceUpdateId,
                  host: &ot::SrpServerHost,
                  timeout: u32| {
                debug!(
                    tag = "srp_advertising_proxy";
                    "srp_server_set_service_update: Update for {:?}, timeout: {}", host, timeout
                );
                let result = inner.lock().push_srp_host_changes(
                    inner.clone(),
                    instance,
                    host,
                    Some(update_id.clone()),
                );

                if let Err(err) = &result {
                    warn!(
                        tag = "srp_advertising_proxy";
                        "srp_server_set_service_update: Error setting up update for {:?}: {:?}",
                        host,
                        err
                    );
                    // Only report error immediately if setup failed
                    let _ = inner.lock().outstanding_updates.remove(&update_id);
                    ot_instance
                        .srp_server_handle_service_update_result(update_id, Err(ot::Error::Failed));
                } else {
                    debug!(
                        tag = "srp_advertising_proxy";
                        "srp_server_set_service_update: Started publishing {:?}", host
                    );
                    // Succeed - result will be reported later via on_mdns_publish_result
                }
            },
        ));

        info!(tag = "srp_advertising_proxy"; "AdvertisingProxy Started");

        Ok(ret)
    }

    /// Processes an mDNS result message received from the channel
    pub(crate) fn process_mdns_result(&self, instance: &ot::Instance, msg: MdnsResultMessage) {
        self.inner.lock().on_mdns_publish_result(msg.update_id, msg.result, |id, err| {
            instance.srp_server_handle_service_update_result(id, err)
        });
    }
}

#[derive(Debug)]
pub struct MdnsResultPoller<'a, T: ?Sized>(&'a T);

impl<'a, T: MdnsResultPollerExt + ?Sized> futures::Future for MdnsResultPoller<'a, T> {
    type Output = Result<(), anyhow::Error>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.mdns_result_poll(cx)
    }
}

pub trait MdnsResultPollerExt {
    fn mdns_result_poll(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), anyhow::Error>>;

    fn mdns_result_future(&self) -> MdnsResultPoller<'_, Self> {
        MdnsResultPoller(self)
    }
}

impl<T: AsRef<ot::Instance> + AsRef<Option<AdvertisingProxy>>> MdnsResultPollerExt
    for fuchsia_sync::Mutex<T>
{
    fn mdns_result_poll(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), anyhow::Error>> {
        let guard = self.lock();

        let ot_instance: &ot::Instance = guard.as_ref();
        let advertising_proxy_option: &Option<AdvertisingProxy> = guard.as_ref();

        if let Some(advertising_proxy) = advertising_proxy_option {
            // Try to receive messages from the channel
            let mut receiver_guard = advertising_proxy.mdns_result_receiver.lock();
            if let Some(receiver) = receiver_guard.as_mut() {
                // Poll the receiver for new messages
                loop {
                    match receiver.poll_next_unpin(cx) {
                        std::task::Poll::Ready(Some(msg)) => {
                            // Process the message
                            advertising_proxy.process_mdns_result(ot_instance, msg)
                        }
                        std::task::Poll::Ready(None) => {
                            // Channel closed
                            return std::task::Poll::Ready(Err(anyhow::anyhow!(
                                "mDNS result channel closed"
                            )));
                        }
                        std::task::Poll::Pending => {
                            // No messages available, keep polling
                            return std::task::Poll::Pending;
                        }
                    }
                }
            } else {
                // No receiver available
                return std::task::Poll::Ready(Ok(()));
            }
        }
        // No advertising proxy available
        std::task::Poll::Ready(Ok(()))
    }
}

impl AdvertisingProxyInner {
    /// Handles mDNS publish results and updates outstanding update tracking.
    fn on_mdns_publish_result<F: FnOnce(ot::SrpServerServiceUpdateId, Result<(), ot::Error>)>(
        &mut self,
        update_id: ot::SrpServerServiceUpdateId,
        result: Result<(), ot::Error>,
        handle_result: F,
    ) {
        if let Some(update) = self.outstanding_updates.get_mut(&update_id) {
            update.callback_count -= 1;
            if result.is_err() || update.callback_count == 0 {
                // Remove the update before notifying OpenThread
                let update = self.outstanding_updates.remove(&update_id).unwrap();

                debug!(
                    tag = "srp_advertising_proxy";
                    "Completing SRP update {:?} for host {:?} with result {:?}",
                    update_id, update.host_name, result
                );

                handle_result(update_id, result);
            } else {
                // Still waiting for more callbacks
                debug!(
                    tag = "srp_advertising_proxy";
                    "Waiting for {} more mDNS operations for update {:?}",
                    update.callback_count, update_id
                );
            }
        } else {
            warn!(
                tag = "srp_advertising_proxy";
                "Received mDNS result for unknown update_id {:?}, potentially due to other \
                requests failed", update_id
            );
        }
    }

    fn finish_proxy_host_publish(
        inner_arc: &Arc<Mutex<Self>>,
        host_full_name: &CStr,
        service_publisher: ServiceInstancePublisherProxy,
        result: Result<(), ot::Error>,
        client_to_update: Option<fidl::endpoints::ClientEnd<ServiceInstancePublisherMarker>>,
        tx_opt: &mut Option<futures::channel::oneshot::Sender<()>>,
        tracker: Option<UpdateTracker>,
    ) {
        // Send result via channel if we're tracking this update.
        if let Some(tracker) = tracker {
            tracker.resolve(result);
        }

        let mut inner = inner_arc.lock();
        let is_same_host = inner.hosts.get(host_full_name).is_some_and(|host| {
            std::ptr::eq(host.service_publisher.as_channel(), service_publisher.as_channel())
        });
        if !is_same_host {
            return;
        }

        if result.is_ok() {
            if let (Some(c), Some(host)) = (client_to_update, inner.hosts.get_mut(host_full_name)) {
                host.service_publisher = c.into_proxy();
            }
            // Notify waiting service tasks that the host publisher is ready.
            if let Some(tx) = tx_opt.take() {
                let _ = tx.send(());
            }
            return;
        }

        // Remove the failed host and resolve any pending `Publishing` service
        // `UpdateTracker`s with `result` so specific errors like `Duplicated` are preserved
        // instead of falling back to `UpdateTracker::drop`'s `Err(ot::Error::Failed)`.
        let Some(host) = inner.hosts.remove(host_full_name) else {
            return;
        };
        drop(inner);

        for service in host.services.values() {
            let AdvertisingProxyServiceState::Publishing { tracker, .. } = service else {
                continue;
            };
            if let Some(tracker) = tracker.lock().take() {
                tracker.resolve(result);
            }
        }
    }

    async fn wait_for_host_publisher(
        inner_arc: &Arc<Mutex<Self>>,
        publisher_notifier: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
        host_full_name: &CStr,
        instance_name: &str,
    ) -> Option<ServiceInstancePublisherProxy> {
        if publisher_notifier.await.is_err() {
            debug!(
                tag = "srp_advertising_proxy";
                "Host {:?} failed to publish, skipping service {:?} publication",
                host_full_name,
                instance_name
            );
            return None;
        }

        let inner = inner_arc.lock();
        inner.hosts.get(host_full_name).map(|host| host.service_publisher.clone())
    }

    pub fn publish_srp_all(
        &mut self,
        inner: Arc<Mutex<Self>>,
        instance: &ot::Instance,
    ) -> Result<(), anyhow::Error> {
        for host in instance.srp_server_hosts() {
            if let Err(err) = self.push_srp_host_changes(inner.clone(), instance, host, None) {
                warn!(
                    tag = "srp_advertising_proxy";
                    "Unable to fully publish SRP host {:?} to mDNS: {:?}",
                    host.full_name_cstr(),
                    err
                );
            }
        }

        Ok(())
    }

    /// Makes sure that the proxy host publisher is working.
    pub fn verify_mdns_connection(&mut self) -> Result<(), anyhow::Error> {
        if self.mdns_proxy_host_publisher.is_closed() {
            warn!(
                tag = "srp_advertising_proxy";
                "self.mdns_proxy_host_publisher has closed unexpectedly."
            );

            self.mdns_proxy_host_publisher = connect_to_protocol::<ProxyHostPublisherMarker>()
                .context("mdns_proxy_host_publisher")?;
        }

        Ok(())
    }

    /// Updates the mDNS service with the host and services from the SrpServerHost.
    /// If update_id is provided, tracks the operations and defers result reporting.
    pub fn push_srp_host_changes<'a>(
        &mut self,
        inner: Arc<Mutex<Self>>,
        instance: &'a ot::Instance,
        mut srp_host: &'a ot::SrpServerHost,
        update_id: Option<ot::SrpServerServiceUpdateId>,
    ) -> Result<(), anyhow::Error> {
        // Cache the mesh local prefix for use in the closure below.
        let mesh_local_prefix = *instance.get_mesh_local_prefix();

        // Prepare the list of addresses associated with this host.
        let addresses = srp_host
            .addresses()
            .iter()
            .copied()
            .filter(|x| {
                !net_types::ip::Ipv6Addr::from_bytes(x.octets()).is_unicast_link_local()
                    && !mesh_local_prefix.contains(x)
            })
            .collect::<Vec<_>>();

        let local_name = srp_host
            .full_name_cstr()
            .as_ref()
            .to_str()?
            .trim_end_matches(&self.srp_domain)
            .trim_end_matches('.');

        if srp_host.is_deleted() {
            // Delete the host.
            info!(
                tag = "srp_advertising_proxy";
                "No longer advertising host [PII]({:?}) on {:?}",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN
            );

            let local_name = local_name.to_string();

            if let Some(update_id) = update_id {
                let host_to_drop = self.hosts.remove(srp_host.full_name_cstr());

                if host_to_drop.is_none() {
                    instance.srp_server_handle_service_update_result(update_id, Ok(()));
                    return Ok(());
                }

                let sender = self.mdns_result_sender.clone();

                self.outstanding_updates
                    .entry(update_id.clone())
                    .or_insert_with(|| OutstandingUpdate {
                        update_id: update_id.clone(),
                        host_name: srp_host.full_name_cstr().to_owned(),
                        callback_count: 0,
                    })
                    .callback_count += 1;

                fasync::Task::spawn(async move {
                    let result = match futures::future::select(
                        Box::pin(fasync::Timer::new(UNPUBLISH_SYNC_TIMEOUT)),
                        Box::pin(wait_for_host_removal(local_name, host_to_drop)),
                    )
                    .await
                    {
                        futures::future::Either::Left(_) => {
                            warn!(tag = "srp_advertising_proxy"; "Timeout waiting for host removal");
                            Err(ot::Error::Failed)
                        }
                        futures::future::Either::Right((Ok(()), _)) => Ok(()),
                        futures::future::Either::Right((Err(err), _)) => {
                            warn!(
                                tag = "srp_advertising_proxy";
                                "Error waiting for host removal: {:?}",
                                err
                            );
                            Err(ot::Error::Failed)
                        }
                    };

                    let _ = sender.unbounded_send(MdnsResultMessage { update_id, result });
                })
                .detach();
            }
            return Ok(());
        }

        // If there is already a host, check to make sure the addresses match (<https://fxbug.dev/42066484>)
        // and that the service publisher FIDL is not closed.
        if let Some(host) = self.hosts.get_mut(srp_host.full_name_cstr()) {
            if host.addresses != addresses {
                // Addresses do not match.
                info!(
                    tag = "srp_advertising_proxy";
                    "IP addresses for host [PII]({:?}) has changed. Was {:?}, now {:?}.",
                    srp_host.full_name_cstr(),
                    host.addresses,
                    addresses
                );
                // Delete the host so we can re-create it below.
                self.hosts.remove(srp_host.full_name_cstr());
            } else if host.service_publisher.is_closed() {
                // The service publisher was closed for some reason. We will need
                // to re-open it before we can update any services.
                warn!(
                    tag="srp_advertising_proxy"; "ServiceInstancePublisherProxy for host [PII]({:?}) was closed. Will restart it.",
                    srp_host.full_name_cstr()
                );
                // Delete the host so we can re-create it below.
                self.hosts.remove(srp_host.full_name_cstr());
            }
        }

        if let Some(ref update_id) = update_id {
            let outstanding_update = OutstandingUpdate {
                update_id: update_id.clone(),
                host_name: srp_host.full_name_cstr().to_owned(),
                callback_count: 0,
            };

            debug!(
                tag = "srp_advertising_proxy";
                "Tracking update {:?} for host {:?}",
                update_id, srp_host.full_name_cstr()
            );

            self.outstanding_updates.insert(update_id.clone(), outstanding_update);
        }

        let host: &mut AdvertisingProxyHost = if let Some(host) =
            self.hosts.get_mut(srp_host.full_name_cstr())
        {
            // Use the existing host.
            debug!(
                tag = "srp_advertising_proxy";
                "Updating advertisement of {:?} on {:?}",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN
            );

            host
        } else {
            // Add the host.
            info!(
                tag = "srp_advertising_proxy";
                "Advertising host [PII]({:?}) on {:?} as [PII]({:?})",
                srp_host.full_name_cstr(),
                LOCAL_DOMAIN,
                local_name
            );

            if local_name.len() > MAX_DNSSD_HOST_LEN {
                bail!("Host {:?} is too long (max {} chars)", local_name, MAX_DNSSD_HOST_LEN);
            }

            // Warn if the hostname only contains legal characters.
            if local_name.starts_with('-')
                || local_name.contains(|ch: char| {
                    !(ch.is_ascii_alphanumeric() || ch == '-' || !ch.is_ascii())
                })
            {
                warn!(
                    tag = "srp_advertising_proxy";
                    "Host [PII]({local_name:?}) contains forbidden characters"
                );
            }

            let (client, server) = create_endpoints::<ServiceInstancePublisherMarker>();
            let service_publisher = client.into_proxy();
            let service_publisher_for_future = service_publisher.clone();

            // This is copied just for use in error messages below.
            let local_name_copy = local_name.to_string();

            // Prepare versions of the addresses for use in FIDL call.
            let addrs = addresses
                .iter()
                .map(|x| {
                    fidl_fuchsia_net::IpAddress::Ipv6(fidl_fuchsia_net::Ipv6Address {
                        addr: x.octets(),
                    })
                })
                .collect::<Vec<_>>();

            // Make sure that the connection to the mDNS component is solid.
            self.verify_mdns_connection()?;

            // Get sender for callbacks
            let sender = self.mdns_result_sender.clone();
            let mut tracker = None;
            if let Some(ref update_id) = update_id {
                if let Some(update) = self.outstanding_updates.get_mut(update_id) {
                    update.callback_count += 1;
                }
                tracker = Some(UpdateTracker::new(sender, Some(update_id.clone())));
            }
            let publisher = self.mdns_proxy_host_publisher.clone();
            let local_name_str = local_name_copy;
            let addrs = addrs;
            let inner_arc = inner.clone();
            let host_full_name = srp_host.full_name_cstr().to_owned();

            let (tx, rx) = futures::channel::oneshot::channel::<()>();
            let rx_shared = rx.shared();
            let mut tx_opt = Some(tx);

            let publish_proxy_host_future = async move {
                let mut retries_left = MAX_PUBLISH_RETRIES;
                let mut server_to_use = Some(server);
                let mut client_to_update = None;

                loop {
                    let s = match server_to_use.take() {
                        Some(s) => s,
                        None => {
                            let (c, s) = create_endpoints::<ServiceInstancePublisherMarker>();
                            client_to_update = Some(c);
                            s
                        }
                    };

                    let x = publisher
                        .publish_proxy_host(
                            &local_name_str,
                            &addrs,
                            &ProxyHostPublicationOptions {
                                perform_probe: Some(true),
                                ..Default::default()
                            },
                            s,
                        )
                        .await;

                    let result = match x {
                        Ok(Ok(())) => {
                            debug!(tag = "srp_advertising_proxy"; "publish_proxy_host: success");
                            Ok(())
                        }
                        Ok(Err(PublishProxyHostError::AlreadyPublishedLocally))
                            if retries_left > 0 =>
                        {
                            warn!(
                                tag = "srp_advertising_proxy";
                                "Host {:?} already published locally, retrying...", local_name_str
                            );

                            let timeout = fasync::Timer::new(UNPUBLISH_SYNC_TIMEOUT);
                            let wait_fut = wait_for_host_removal(
                                local_name_str.clone(),
                                None::<AdvertisingProxyHost>,
                            );
                            let result = match futures::future::select(
                                Box::pin(timeout),
                                Box::pin(wait_fut),
                            )
                            .await
                            {
                                futures::future::Either::Left(_) => {
                                    Err(anyhow::anyhow!("Timeout waiting for host removal"))
                                }
                                futures::future::Either::Right((res, _)) => res,
                            };
                            if let Err(e) = result {
                                warn!(tag = "srp_advertising_proxy"; "Error waiting for mDNS host \
                                removal: {:?}", e);
                            }

                            retries_left -= 1;
                            continue;
                        }
                        Ok(Err(
                            err @ (PublishProxyHostError::AlreadyPublishedLocally
                            | PublishProxyHostError::AlreadyPublishedOnSubnet),
                        )) => {
                            warn!(
                                tag = "srp_advertising_proxy";
                                "publish_proxy_host: {:?} already published: {:?}",
                                local_name_str,
                                err
                            );
                            Err(ot::Error::Duplicated)
                        }
                        Ok(Err(err)) => {
                            error!(tag = "srp_advertising_proxy"; "publish_proxy_host: {:?}", err);
                            Err(ot::Error::Failed)
                        }
                        Err(err) => {
                            error!(tag = "srp_advertising_proxy"; "publish_proxy_host: {:?}", err);
                            Err(ot::Error::Failed)
                        }
                    };

                    Self::finish_proxy_host_publish(
                        &inner_arc,
                        &host_full_name,
                        service_publisher_for_future,
                        result,
                        client_to_update,
                        &mut tx_opt,
                        tracker,
                    );
                    return;
                }
            };

            self.hosts.insert(
                srp_host.full_name_cstr().to_owned(),
                AdvertisingProxyHost {
                    services: HashMap::new(),
                    service_publisher,
                    addresses,
                    publisher_notifier: rx_shared,
                },
            );

            fuchsia_async::Task::spawn(publish_proxy_host_future).detach();

            // If there are no services in this update, then grab the "real" `ot::SrpServerHost`,
            // because this is probably a delta. Since we are perform the initial setup for this
            // host, we cannot use a delta update.
            if srp_host.services().count() == 0 {
                for real_host in instance.srp_server_hosts() {
                    if srp_host.full_name_cstr() == real_host.full_name_cstr() {
                        info!(
                            tag = "srp_advertising_proxy";
                            "Using [PII]({:?}) instead of [PII]({:?}).", real_host, srp_host
                        );

                        srp_host = real_host;
                        break;
                    }
                }
            }

            self.hosts.get_mut(srp_host.full_name_cstr()).unwrap()
        };

        let services = &mut host.services;
        let mut seen_services = HashSet::new();

        // Handle adding/removing whole services
        for srp_service in srp_host.services() {
            seen_services.insert(srp_service.instance_name_cstr().to_owned());

            // The service name as a Rust string slice from the SRP service.
            let service_name = srp_service.service_name_cstr().as_ref().to_str()?;

            // The service name without the domain, with a trailing period, like "_trel._udp.".
            let local_service_name = service_name.trim_end_matches(&self.srp_domain);

            // The instance name without the service name or domain,
            // without any trailing period, like "My-Service".
            let local_instance_name = srp_service
                .instance_name_cstr()
                .as_ref()
                .to_str()?
                .trim_end_matches(service_name)
                .trim_end_matches('.');

            if srp_service.is_deleted() {
                // Delete the service.
                if let Some(service_to_drop) = services.remove(srp_service.instance_name_cstr()) {
                    debug!(
                        tag = "srp_advertising_proxy";
                        "No longer advertising service {:?} on {:?}",
                        srp_service.instance_name_cstr(),
                        LOCAL_DOMAIN
                    );

                    if let Some(ref update_id) = update_id {
                        let sender = self.mdns_result_sender.clone();
                        let update_id_clone = update_id.clone();
                        let service_type = local_service_name.to_string();
                        let instance_name = local_instance_name.to_string();

                        if let Some(update) = self.outstanding_updates.get_mut(update_id) {
                            update.callback_count += 1;
                        } else {
                            warn!(tag = "srp_advertising_proxy"; "Update ID {:?} not found in \
                            outstanding_updates", update_id);
                        }

                        fasync::Task::spawn(async move {
                            let result = match futures::future::select(
                                Box::pin(fasync::Timer::new(UNPUBLISH_SYNC_TIMEOUT)),
                                Box::pin(wait_for_service_removal(
                                    service_type,
                                    instance_name,
                                    Some(service_to_drop),
                                )),
                            )
                            .await
                            {
                                futures::future::Either::Left(_) => {
                                    warn!(
                                        tag = "srp_advertising_proxy";
                                        "Timeout waiting for service removal"
                                    );
                                    Err(ot::Error::Failed)
                                }
                                futures::future::Either::Right((Ok(()), _)) => Ok(()),
                                futures::future::Either::Right((Err(err), _)) => {
                                    warn!(
                                        tag = "srp_advertising_proxy";
                                        "Error waiting for service removal: {:?}",
                                        err
                                    );
                                    Err(ot::Error::Failed)
                                }
                            };

                            let _ = sender.unbounded_send(MdnsResultMessage {
                                update_id: update_id_clone,
                                result,
                            });
                        })
                        .detach();
                    }
                }
                continue;
            }

            let service_name = srp_service.instance_name_cstr().to_owned();

            let subtypes: Vec<String> = srp_service
                .subtypes()
                .filter_map(|x: &CStr| match x.to_str() {
                    Ok(x) => Some(x[0..x.find('.').unwrap_or(x.len())].to_string()),
                    Err(err) => {
                        warn!(
                            tag = "srp_advertising_proxy";
                            "Unacceptable subtype {x:?}: {err:?}"
                        );
                        None
                    }
                })
                .collect();

            if let Some(service) = services.get(&service_name) {
                let service_info = AdvertisingProxyServiceInfo::new(srp_service);
                let mut updated = false;
                if !service.is_up_to_date(srp_service) {
                    // Update the service.
                    if let Err(err) = service.update(service_info) {
                        warn!(
                            tag = "srp_advertising_proxy";
                            "Unable to update service {:?}: {:?}. Will try re-adding.",
                            local_service_name,
                            err
                        );
                    } else {
                        debug!(
                            tag = "srp_advertising_proxy";
                            "Updated service {:?} on {:?} as {:?}",
                            local_service_name,
                            LOCAL_DOMAIN,
                            local_instance_name
                        );
                        updated = true;
                    }
                } else {
                    // No update necessary.
                    debug!(
                        tag = "srp_advertising_proxy";
                        "Service {:?} is up to date on {:?} as {:?}",
                        local_service_name,
                        LOCAL_DOMAIN,
                        local_instance_name
                    );
                    updated = true;
                }

                if updated {
                    let subtypes_copy = subtypes.clone();
                    if let Err(err) = service.set_subtypes(subtypes) {
                        warn!(
                            tag = "srp_advertising_proxy";
                            "Can't set subtypes on {service_name:?} to {subtypes_copy:?}: {err:?}"
                        );
                    }
                    // Skip the add.
                    continue;
                }
            }

            // Add the service.
            let service_info = AdvertisingProxyServiceInfo::new(srp_service);

            debug!(
                tag = "srp_advertising_proxy";
                "Adding service {:?} on {:?} as {:?}",
                local_service_name,
                LOCAL_DOMAIN,
                local_instance_name
            );

            if local_service_name.len() > MAX_DNSSD_SERVICE_LEN {
                warn!(
                    tag = "srp_advertising_proxy";
                    "Unable to publish service instance {:?}: Service too long (max {} chars)",
                    local_service_name,
                    MAX_DNSSD_SERVICE_LEN
                );

                if let Some(ref update_id) = update_id {
                    let sender = self.mdns_result_sender.clone();
                    if let Some(update) = self.outstanding_updates.get_mut(update_id) {
                        update.callback_count += 1;
                    } else {
                        warn!(tag = "srp_advertising_proxy"; "Update ID {:?} not found in \
                        outstanding_updates", update_id);
                    }
                    let _ = sender.unbounded_send(MdnsResultMessage {
                        update_id: update_id.clone(),
                        result: Err(ot::Error::Failed),
                    });
                }

                continue;
            }

            if local_instance_name.len() > MAX_DNSSD_INSTANCE_LEN {
                warn!(
                    tag="srp_advertising_proxy"; "Unable to publish service instance {:?}: \
                    Instance name too long (max {} chars)",
                    local_instance_name, MAX_DNSSD_INSTANCE_LEN
                );

                if let Some(ref update_id) = update_id {
                    let sender = self.mdns_result_sender.clone();
                    if let Some(update) = self.outstanding_updates.get_mut(update_id) {
                        update.callback_count += 1;
                    } else {
                        warn!(tag = "srp_advertising_proxy"; "Update ID {:?} not found in \
                        outstanding_updates", update_id);
                    }
                    let _ = sender.unbounded_send(MdnsResultMessage {
                        update_id: update_id.clone(),
                        result: Err(ot::Error::Failed),
                    });
                }

                continue;
            }

            let sender = self.mdns_result_sender.clone();
            let mut tracker = None;
            if let Some(ref update_id) = update_id {
                if let Some(update) = self.outstanding_updates.get_mut(update_id) {
                    update.callback_count += 1;
                }
                tracker = Some(UpdateTracker::new(sender, Some(update_id.clone())));
            }
            let tracker = Arc::new(Mutex::new(tracker));
            let tracker_for_future = tracker.clone();

            let publisher_notifier = host.publisher_notifier.clone();
            let service_type = local_service_name.to_string();
            let instance_name = local_instance_name.to_string();
            let inner_arc = inner.clone();
            let host_full_name = srp_host.full_name_cstr().to_owned();
            let service_instance_name = srp_service.instance_name_cstr().to_owned();
            let service_info_data = service_info;

            let publish_service_future = async move {
                // Wait for the host publisher to fully resolve before attempting to publish
                // services. If the host publisher failed, skip publishing services.
                let Some(publisher) = Self::wait_for_host_publisher(
                    &inner_arc,
                    publisher_notifier,
                    &host_full_name,
                    &instance_name,
                )
                .await
                else {
                    // `wait_for_host_publisher` returns `None` when host publication fails
                    // (`publisher_notifier.await.is_err()`) or the host entry is already gone,
                    // causing `publish_service_future` to return early without publishing.
                    // If `finish_proxy_host_publish` did not already take and resolve this
                    // tracker, resolve it with `Err(ot::Error::Failed)` immediately rather than
                    // waiting for the service entry in `host.services` to be dropped.
                    if let Some(tracker) = tracker_for_future.lock().take() {
                        tracker.resolve(Err(ot::Error::Failed));
                    }
                    return;
                };

                let mut retries_left = MAX_PUBLISH_RETRIES;
                loop {
                    let (client, server) =
                        create_endpoints::<ServiceInstancePublicationResponder_Marker>();
                    let res = publisher
                        .publish_service_instance(
                            &service_type,
                            &instance_name,
                            &ServiceInstancePublicationOptions::default(),
                            client,
                        )
                        .await;

                    let result = match res {
                        Ok(Ok(())) => {
                            debug!(tag = "srp_advertising_proxy"; "publish_service_instance: \
                            success");
                            Ok(())
                        }
                        Ok(Err(PublishServiceInstanceError::AlreadyPublishedLocally))
                            if retries_left > 0 =>
                        {
                            warn!(
                                tag = "srp_advertising_proxy";
                                "Service {:?} already published locally, retrying...",
                                instance_name
                            );

                            let timeout = fasync::Timer::new(UNPUBLISH_SYNC_TIMEOUT);
                            let wait_fut = wait_for_service_removal(
                                service_type.clone(),
                                instance_name.clone(),
                                None::<AdvertisingProxyServiceState>,
                            );
                            let result = match futures::future::select(
                                Box::pin(timeout),
                                Box::pin(wait_fut),
                            )
                            .await
                            {
                                futures::future::Either::Left(_) => {
                                    Err(anyhow::anyhow!("Timeout waiting for service removal"))
                                }
                                futures::future::Either::Right((res, _)) => res,
                            };
                            if let Err(e) = result {
                                warn!(tag = "srp_advertising_proxy"; "Error waiting for mDNS \
                                service removal: {:?}", e);
                            }

                            retries_left -= 1;
                            continue;
                        }
                        Ok(Err(
                            err @ (PublishServiceInstanceError::AlreadyPublishedLocally
                            | PublishServiceInstanceError::AlreadyPublishedOnSubnet),
                        )) => {
                            warn!(
                                tag = "srp_advertising_proxy";
                                "publish_service_instance: {:?} already published: {:?}",
                                instance_name,
                                err
                            );
                            Err(ot::Error::Duplicated)
                        }
                        Err(err) => {
                            error!(
                                tag = "srp_advertising_proxy";
                                "publish_service_instance: {:?}", err
                            );
                            Err(ot::Error::Failed)
                        }
                    };

                    if result.is_ok() {
                        let service_info = Arc::new(Mutex::new(service_info_data.clone()));
                        let service_info_clone = service_info.clone();

                        let (pub_responder_stream, pub_responder_control) =
                            server.into_stream_and_control_handle();

                        let publish_responder_future = pub_responder_stream
                            .map_err(anyhow::Error::from)
                            .try_for_each(
                                move |ServiceInstancePublicationResponder_Request::OnPublication {
                                          responder,
                                          subtype,
                                          publication_cause,
                                          ..
                                      }| {
                                    let service_info = service_info_clone.lock().clone();
                                    let service_name = service_info.name.clone();

                                    let should_skip = if let Some(subtype) = subtype.as_ref() {
                                        if !service_info.has_subtype(subtype) {
                                            true
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    };

                                    let info = service_info.into_service_instance_publication();

                                    let result =
                                        if should_skip {
                                            debug!(
                                                tag = "srp_advertising_proxy";
                                                "publish_responder_future: {:?}, {service_name:?},\
                                                returning DO_NOT_RESPOND, does not have subtype \
                                                {subtype:?}",
                                                publication_cause
                                            );

                                            Err(OnPublicationError::DoNotRespond)
                                        } else {
                                            debug!(
                                                tag = "srp_advertising_proxy";
                                                "publish_responder_future: {:?}, {service_name:?},\
                                                subtype {subtype:?}",
                                                publication_cause
                                            );

                                            Ok(&info)
                                        };

                                    futures::future::ready(
                                        responder.send(result).map_err(anyhow::Error::from),
                                    )
                                },
                            );

                        let mut inner = inner_arc.lock();
                        if let Some(host) = inner.hosts.get_mut(&host_full_name) {
                            let svc = AdvertisingProxyService {
                                info: service_info,
                                control_handle: pub_responder_control,
                                task: fuchsia_async::Task::spawn(
                                    publish_responder_future.map(|_| Ok(())),
                                ),
                            };
                            if !subtypes.is_empty() {
                                if let Err(err) = svc.set_subtypes(subtypes) {
                                    warn!(
                                        tag = "srp_advertising_proxy";
                                        "Can't set subtypes on {service_instance_name:?}: {err:?}"
                                    );
                                }
                            }
                            // The publication to mDNS succeeded. Transition the service
                            // state from `Publishing` to `Active` by replacing the entry.
                            host.services.insert(
                                service_instance_name.clone(),
                                AdvertisingProxyServiceState::Active(svc),
                            );
                        }
                    }

                    // Send result via channel if we're tracking this update
                    if let Some(tracker) = tracker_for_future.lock().take() {
                        tracker.resolve(result);
                    }
                    return;
                }
            };

            // Store the async publication task immediately in the `Publishing` state.
            // If the service is removed while the FIDL call is still in flight, this
            // task will be dropped and aborted, preventing a ghost service.
            let task = fasync::Task::spawn(publish_service_future);
            services
                .insert(service_name, AdvertisingProxyServiceState::Publishing { task, tracker });
        }

        if update_id.is_none() {
            services.retain(|name, _| seen_services.contains(name));
        }

        if let Some(update_id) = update_id {
            if let Some(update) = self.outstanding_updates.get(&update_id) {
                if update.callback_count == 0 {
                    // This update only contained deletions or no-ops, so we can complete it now.
                    let update = self.outstanding_updates.remove(&update_id).unwrap();
                    debug!(
                        tag = "srp_advertising_proxy";
                        "Completing SRP update {:?} for host {:?} with result Ok (deletions/no-ops only)",
                        update_id, update.host_name
                    );
                    instance.srp_server_handle_service_update_result(update_id, Ok(()));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[fuchsia::test]
    async fn test_update_tracker_resolve() {
        let (sender, mut receiver) = mpsc::unbounded();
        // SAFETY: `SrpServerServiceUpdateId` is a `#[repr(transparent)]` wrapper around `u32`.
        let update_id: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(12345u32) };
        let tracker = UpdateTracker::new(sender, Some(update_id.clone()));

        tracker.resolve(Ok(()));

        let message = receiver.next().await.expect("should receive message");
        assert_eq!(message.update_id, update_id);
        assert_eq!(message.result, Ok(()));
    }

    #[fuchsia::test]
    async fn test_update_tracker_drop() {
        let (sender, mut receiver) = mpsc::unbounded();
        // SAFETY: `SrpServerServiceUpdateId` is a `#[repr(transparent)]` wrapper around `u32`.
        let update_id: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(54321u32) };
        {
            let _tracker = UpdateTracker::new(sender, Some(update_id.clone()));
        }

        let message = receiver.next().await.expect("should receive message");
        assert_eq!(message.update_id, update_id);
        assert_eq!(message.result, Err(ot::Error::Failed));
    }

    #[fuchsia::test]
    async fn test_multiple_services_callback() {
        let (mdns_proxy_host_publisher, _) =
            fidl::endpoints::create_proxy::<ProxyHostPublisherMarker>();
        let (mdns_result_sender, _) = mpsc::unbounded();

        let mut inner = AdvertisingProxyInner {
            srp_domain: "default.service.arpa.".to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher,
            outstanding_updates: HashMap::new(),
            mdns_result_sender,
        };

        // SAFETY: `SrpServerServiceUpdateId` is a `#[repr(transparent)]` wrapper around `u32`.
        let update_id: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(42u32) };
        let cstr_hostname = CString::new("my-host").expect("valid CString");

        // Simulate 1 host + 2 services being tracked
        inner.outstanding_updates.insert(
            update_id.clone(),
            OutstandingUpdate {
                update_id: update_id.clone(),
                host_name: cstr_hostname,
                callback_count: 3,
            },
        );

        let callback_fires = Rc::new(Cell::new(0));
        let error_received = Rc::new(Cell::new(None));

        // First resolve: service 1
        let fires_clone1 = callback_fires.clone();
        inner.on_mdns_publish_result(update_id.clone(), Ok(()), |_, _| {
            fires_clone1.set(fires_clone1.get() + 1);
        });
        assert_eq!(callback_fires.get(), 0, "Callback should not fire until completely resolved");

        // Second resolve: service 2
        let fires_clone2 = callback_fires.clone();
        inner.on_mdns_publish_result(update_id.clone(), Ok(()), |_, _| {
            fires_clone2.set(fires_clone2.get() + 1);
        });
        assert_eq!(callback_fires.get(), 0, "Callback should not fire until completely resolved");

        // Third resolve: host
        let fires_clone3 = callback_fires.clone();
        let error_clone = error_received.clone();
        inner.on_mdns_publish_result(update_id.clone(), Ok(()), |_, err| {
            fires_clone3.set(fires_clone3.get() + 1);
            error_clone.set(Some(err));
        });
        assert_eq!(callback_fires.get(), 1, "Callback should have fired exactly once now");
        assert_eq!(error_received.get(), Some(Ok(())), "Expected Ok result");
    }

    #[fuchsia::test]
    async fn test_duplicate_error_propagated_immediately() {
        let (mdns_proxy_host_publisher, _) =
            fidl::endpoints::create_proxy::<ProxyHostPublisherMarker>();
        let (mdns_result_sender, _) = mpsc::unbounded();

        let mut inner = AdvertisingProxyInner {
            srp_domain: "default.service.arpa.".to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher,
            outstanding_updates: HashMap::new(),
            mdns_result_sender,
        };

        // SAFETY: `SrpServerServiceUpdateId` is a `#[repr(transparent)]` wrapper around `u32`.
        let update_id: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(99u32) };
        let cstr_hostname = CString::new("conflict-host").expect("valid CString");

        // Simulate 1 host + 1 service being tracked
        inner.outstanding_updates.insert(
            update_id.clone(),
            OutstandingUpdate {
                update_id: update_id.clone(),
                host_name: cstr_hostname,
                callback_count: 2,
            },
        );

        let callback_fires = Rc::new(Cell::new(0));
        let error_received = Rc::new(Cell::new(None));

        let fires_clone = callback_fires.clone();
        let error_clone = error_received.clone();
        inner.on_mdns_publish_result(update_id.clone(), Err(ot::Error::Duplicated), |_, err| {
            fires_clone.set(fires_clone.get() + 1);
            error_clone.set(Some(err));
        });
        assert_eq!(callback_fires.get(), 1, "Callback should fire immediately on error");
        assert_eq!(
            error_received.get(),
            Some(Err(ot::Error::Duplicated)),
            "Expected Err(ot::Error::Duplicated)"
        );
        assert!(
            !inner.outstanding_updates.contains_key(&update_id),
            "Outstanding update should be removed after error"
        );
    }

    #[fuchsia::test]
    async fn test_skip_service_when_host_publish_fails() {
        let (mdns_proxy_host_publisher, _) =
            fidl::endpoints::create_proxy::<ProxyHostPublisherMarker>();
        let (mdns_result_sender, mut mdns_result_receiver) = mpsc::unbounded();

        let inner_arc = Arc::new(Mutex::new(AdvertisingProxyInner {
            srp_domain: "default.service.arpa.".to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher,
            outstanding_updates: HashMap::new(),
            mdns_result_sender: mdns_result_sender.clone(),
        }));

        // SAFETY: `SrpServerServiceUpdateId` is a `#[repr(transparent)]` wrapper around `u32`.
        let update_id: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(101u32) };
        let update_id_2: ot::SrpServerServiceUpdateId = unsafe { std::mem::transmute(102u32) };
        let host_full_name =
            CString::new("conflict-host.default.service.arpa.").expect("valid CString");
        let service_full_name =
            CString::new("my-service._test._tcp.default.service.arpa.").expect("valid CString");
        let service_2_full_name =
            CString::new("my-service-2._test._tcp.default.service.arpa.").expect("valid CString");

        let (service_publisher, mut service_publisher_stream) =
            fidl::endpoints::create_proxy_and_stream::<ServiceInstancePublisherMarker>();
        let service_publisher_for_future = service_publisher.clone();
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        let rx_shared = rx.shared();
        let mut tx_opt = Some(tx);

        // Track 1 host + 1 service publication in `update_id`, plus a second service-only
        // update (`update_id_2`) that arrived while the host was still probing.
        let host_tracker = UpdateTracker::new(mdns_result_sender.clone(), Some(update_id.clone()));
        let service_tracker = Arc::new(Mutex::new(Some(UpdateTracker::new(
            mdns_result_sender.clone(),
            Some(update_id.clone()),
        ))));
        let service_tracker_for_future = service_tracker.clone();

        let service_2_tracker = Arc::new(Mutex::new(Some(UpdateTracker::new(
            mdns_result_sender,
            Some(update_id_2.clone()),
        ))));
        let service_2_tracker_for_future = service_2_tracker.clone();

        let inner_for_service = inner_arc.clone();
        let host_name_for_service = host_full_name.clone();
        let notifier_for_service = rx_shared.clone();
        let service_published = Arc::new(Mutex::new(false));
        let service_published_clone = service_published.clone();

        let service_task = fasync::Task::spawn(async move {
            let Some(_publisher) = AdvertisingProxyInner::wait_for_host_publisher(
                &inner_for_service,
                notifier_for_service,
                &host_name_for_service,
                "my-service",
            )
            .await
            else {
                if let Some(tracker) = service_tracker_for_future.lock().take() {
                    tracker.resolve(Err(ot::Error::Failed));
                }
                return;
            };
            *service_published_clone.lock() = true;
        });

        let inner_for_service_2 = inner_arc.clone();
        let host_name_for_service_2 = host_full_name.clone();
        let notifier_for_service_2 = rx_shared.clone();
        let service_2_task = fasync::Task::spawn(async move {
            let Some(_publisher) = AdvertisingProxyInner::wait_for_host_publisher(
                &inner_for_service_2,
                notifier_for_service_2,
                &host_name_for_service_2,
                "my-service-2",
            )
            .await
            else {
                if let Some(tracker) = service_2_tracker_for_future.lock().take() {
                    tracker.resolve(Err(ot::Error::Failed));
                }
                return;
            };
        });

        {
            let mut inner = inner_arc.lock();
            inner.outstanding_updates.insert(
                update_id.clone(),
                OutstandingUpdate {
                    update_id: update_id.clone(),
                    host_name: host_full_name.clone(),
                    callback_count: 2,
                },
            );
            inner.outstanding_updates.insert(
                update_id_2.clone(),
                OutstandingUpdate {
                    update_id: update_id_2.clone(),
                    host_name: host_full_name.clone(),
                    callback_count: 1,
                },
            );
            let mut services = HashMap::new();
            services.insert(
                service_full_name,
                AdvertisingProxyServiceState::Publishing {
                    task: service_task,
                    tracker: service_tracker,
                },
            );
            services.insert(
                service_2_full_name,
                AdvertisingProxyServiceState::Publishing {
                    task: service_2_task,
                    tracker: service_2_tracker,
                },
            );
            inner.hosts.insert(
                host_full_name.clone(),
                AdvertisingProxyHost {
                    services,
                    service_publisher,
                    addresses: Vec::new(),
                    publisher_notifier: rx_shared,
                },
            );
        }

        // Simulate proxy host publication failing with `AlreadyPublishedOnSubnet` -> `Duplicated`,
        // then dropping `tx_opt` when `publish_proxy_host_future` returns.
        AdvertisingProxyInner::finish_proxy_host_publish(
            &inner_arc,
            &host_full_name,
            service_publisher_for_future,
            Err(ot::Error::Duplicated),
            None,
            &mut tx_opt,
            Some(host_tracker),
        );
        drop(tx_opt);

        // The failed host and its pending service state should be removed immediately.
        assert!(
            !inner_arc.lock().hosts.contains_key(&host_full_name),
            "Failed host should be removed from inner.hosts"
        );

        // The service publisher stream should close without receiving any publish requests.
        assert!(
            service_publisher_stream.next().await.is_none(),
            "ServiceInstancePublisher should not receive any requests when host publish fails"
        );
        assert!(
            !*service_published.lock(),
            "Service task must skip publishing when host publish fails"
        );

        // Process result messages and verify both SRP updates fail with `Duplicated`
        // rather than reporting `Ok(())` from `UpdateTracker::drop`.
        let mut results = HashMap::new();
        while let Ok(msg) = mdns_result_receiver.try_recv() {
            inner_arc.lock().on_mdns_publish_result(msg.update_id, msg.result, |id, res| {
                assert!(
                    results.insert(id, res).is_none(),
                    "Each update_id should fire at most once"
                );
            });
        }

        assert_eq!(results.get(&update_id), Some(&Err(ot::Error::Duplicated)));
        assert_eq!(results.get(&update_id_2), Some(&Err(ot::Error::Duplicated)));
    }

    #[fuchsia::test]
    async fn test_failed_host_publish_does_not_remove_newer_host() {
        let (mdns_proxy_host_publisher, _) =
            fidl::endpoints::create_proxy::<ProxyHostPublisherMarker>();
        let (mdns_result_sender, _mdns_result_receiver) = mpsc::unbounded();

        let inner_arc = Arc::new(Mutex::new(AdvertisingProxyInner {
            srp_domain: "default.service.arpa.".to_string(),
            hosts: Default::default(),
            mdns_proxy_host_publisher,
            outstanding_updates: HashMap::new(),
            mdns_result_sender,
        }));

        let host_full_name =
            CString::new("republished-host.default.service.arpa.").expect("valid CString");

        let (old_service_publisher, _old_stream) =
            fidl::endpoints::create_proxy_and_stream::<ServiceInstancePublisherMarker>();
        let (new_service_publisher, _new_stream) =
            fidl::endpoints::create_proxy_and_stream::<ServiceInstancePublisherMarker>();
        let new_service_publisher_check = new_service_publisher.clone();

        let (mut old_tx_opt, _old_rx) = {
            let (tx, rx) = futures::channel::oneshot::channel::<()>();
            (Some(tx), rx.shared())
        };
        let (_new_tx, new_rx) = futures::channel::oneshot::channel::<()>();

        // Insert the newer host instance into `inner.hosts` (simulating OpenThread
        // re-publishing the host while the old host's probe was still in flight).
        inner_arc.lock().hosts.insert(
            host_full_name.clone(),
            AdvertisingProxyHost {
                services: HashMap::new(),
                service_publisher: new_service_publisher,
                addresses: Vec::new(),
                publisher_notifier: new_rx.shared(),
            },
        );

        // Now the old host's publish finishes with an error.
        AdvertisingProxyInner::finish_proxy_host_publish(
            &inner_arc,
            &host_full_name,
            old_service_publisher,
            Err(ot::Error::Failed),
            None,
            &mut old_tx_opt,
            None,
        );

        // The newer host in `inner.hosts` must remain intact.
        let inner = inner_arc.lock();
        let current_host =
            inner.hosts.get(&host_full_name).expect("Newer host must not be removed");
        assert!(std::ptr::eq(
            current_host.service_publisher.as_channel(),
            new_service_publisher_check.as_channel()
        ));
    }
}
