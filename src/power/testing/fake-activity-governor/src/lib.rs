// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake implementation and test base for `fuchsia.power.system/ActivityGovernor` using `fidl_next`.
//!
//! This crate provides two ways to mock `fuchsia.power.system/ActivityGovernor` in unit and driver
//! tests without breaking when new methods are added to the FIDL protocol:
//!
//! 1. **Ready-to-use wake lease fake ([`FakeActivityGovernor`] / [`FakeActivityGovernorServer`]):**
//!    Handles `AcquireWakeLease`, `AcquireUnmonitoredWakeLease`, and `AcquireWakeLeaseWithToken`
//!    with a configurable [`FakeWakeLeaseBehavior`] (`Grant`, `Error`, `Close`, or
//!    `CloseWithEpitaph`), tracking the call count and forwarding granted `server_token` handles
//!    over a channel.
//!
//!    ```ignore
//!    let (client, mut sag) =
//!        FakeActivityGovernor::spawn(&scope.to_handle(), FakeWakeLeaseBehavior::Grant);
//!    ```
//!
//! 2. **Extensible test base trait ([`ActivityGovernorTestServerHandler`] +
//!    [`ActivityGovernorTestServer`]):**
//!    Provides default (`unimplemented!()`) implementations for every `ActivityGovernor` method,
//!    similar to C++'s `fidl::testing::TestBase<ActivityGovernor>`. Tests that need custom
//!    behavior for specific methods can implement [`ActivityGovernorTestServerHandler`] and wrap
//!    their handler in [`ActivityGovernorTestServer`].

use fidl_next_fuchsia_power_system as fsystem;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A base server handler trait for `fuchsia.power.system/ActivityGovernor` with default
/// implementations for every method.
///
/// Similar to `fidl::testing::TestBase<ActivityGovernor>` in C++, implementing this trait
/// (and wrapping the handler in [`ActivityGovernorTestServer`]) insulates test fakes from
/// compile-time breakages when new methods are added to `fuchsia.power.system/ActivityGovernor`.
pub trait ActivityGovernorTestServerHandler<
    T: fidl_next::Transport = fidl_next::fuchsia::zx::Channel,
>: Send
{
    /// Called by default method implementations when an unimplemented method is invoked.
    ///
    /// # Panics
    ///
    /// The default implementation always panics with `unimplemented!()`.
    fn not_implemented(&mut self, method_name: &str) {
        unimplemented!("ActivityGovernor::{method_name} is not implemented");
    }

    fn get_power_elements(
        &mut self,
        _responder: fidl_next::Responder<fsystem::activity_governor::GetPowerElements, T>,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("get_power_elements");
        }
    }

    fn acquire_wake_lease(
        &mut self,
        _request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLease, T>,
        _responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLease, T>,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("acquire_wake_lease");
        }
    }

    fn acquire_unmonitored_wake_lease(
        &mut self,
        _request: fidl_next::Request<fsystem::activity_governor::AcquireUnmonitoredWakeLease, T>,
        _responder: fidl_next::Responder<
            fsystem::activity_governor::AcquireUnmonitoredWakeLease,
            T,
        >,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("acquire_unmonitored_wake_lease");
        }
    }

    fn acquire_wake_lease_with_token(
        &mut self,
        _request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLeaseWithToken, T>,
        _responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLeaseWithToken, T>,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("acquire_wake_lease_with_token");
        }
    }

    fn take_application_activity_lease(
        &mut self,
        _request: fidl_next::Request<fsystem::activity_governor::TakeApplicationActivityLease, T>,
        _responder: fidl_next::Responder<
            fsystem::activity_governor::TakeApplicationActivityLease,
            T,
        >,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("take_application_activity_lease");
        }
    }

    fn register_suspend_blocker(
        &mut self,
        _request: fidl_next::Request<fsystem::activity_governor::RegisterSuspendBlocker, T>,
        _responder: fidl_next::Responder<fsystem::activity_governor::RegisterSuspendBlocker, T>,
    ) -> impl Future<Output = ()> + Send {
        async move {
            self.not_implemented("register_suspend_blocker");
        }
    }

    fn on_unknown_interaction(&mut self, _ordinal: u64) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }
}

/// Adapter that wraps an [`ActivityGovernorTestServerHandler`] to implement
/// [`fsystem::ActivityGovernorServerHandler`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityGovernorTestServer<H>(pub H);

impl<H, T> fsystem::ActivityGovernorServerHandler<T> for ActivityGovernorTestServer<H>
where
    H: ActivityGovernorTestServerHandler<T>,
    T: fidl_next::Transport,
{
    fn get_power_elements(
        &mut self,
        responder: fidl_next::Responder<fsystem::activity_governor::GetPowerElements, T>,
    ) -> impl Future<Output = ()> + Send {
        self.0.get_power_elements(responder)
    }

    fn acquire_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLease, T>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLease, T>,
    ) -> impl Future<Output = ()> + Send {
        self.0.acquire_wake_lease(request, responder)
    }

    fn acquire_unmonitored_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireUnmonitoredWakeLease, T>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireUnmonitoredWakeLease, T>,
    ) -> impl Future<Output = ()> + Send {
        self.0.acquire_unmonitored_wake_lease(request, responder)
    }

    fn acquire_wake_lease_with_token(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLeaseWithToken, T>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLeaseWithToken, T>,
    ) -> impl Future<Output = ()> + Send {
        self.0.acquire_wake_lease_with_token(request, responder)
    }

    fn take_application_activity_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::TakeApplicationActivityLease, T>,
        responder: fidl_next::Responder<
            fsystem::activity_governor::TakeApplicationActivityLease,
            T,
        >,
    ) -> impl Future<Output = ()> + Send {
        self.0.take_application_activity_lease(request, responder)
    }

    fn register_suspend_blocker(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::RegisterSuspendBlocker, T>,
        responder: fidl_next::Responder<fsystem::activity_governor::RegisterSuspendBlocker, T>,
    ) -> impl Future<Output = ()> + Send {
        self.0.register_suspend_blocker(request, responder)
    }

    fn on_unknown_interaction(&mut self, ordinal: u64) -> impl Future<Output = ()> + Send {
        self.0.on_unknown_interaction(ordinal)
    }
}

/// How [`FakeActivityGovernorServer`] responds to wake lease requests (`AcquireWakeLease`,
/// `AcquireUnmonitoredWakeLease`, and `AcquireWakeLeaseWithToken`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeWakeLeaseBehavior {
    /// Grant the lease and forward the `server_token` to the test (if a sender is configured).
    Grant,
    /// Reply with the given domain error.
    Error(fsystem::AcquireWakeLeaseError),
    /// Close the channel without an epitaph and without replying.
    Close,
    /// Close the channel with the given epitaph without replying.
    ///
    /// # Panics
    ///
    /// Panics when handling a wake lease request if [`FakeActivityGovernorServer::with_server`]
    /// was not called to supply the server handle.
    CloseWithEpitaph(zx::Status),
}

/// Fake implementation of `fuchsia.power.system/ActivityGovernor`.
///
/// Serves wake lease acquisition methods (`AcquireWakeLease`, `AcquireUnmonitoredWakeLease`, and
/// `AcquireWakeLeaseWithToken`) according to [`FakeWakeLeaseBehavior`]; every other method panics
/// with `unimplemented!()`.
pub struct FakeActivityGovernorServer {
    behavior: FakeWakeLeaseBehavior,
    call_count: Arc<AtomicUsize>,
    server_tokens: Option<mpsc::UnboundedSender<zx::EventPair>>,
    server: Option<fidl_next::Server<fsystem::ActivityGovernor>>,
}

impl fmt::Debug for FakeActivityGovernorServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeActivityGovernorServer")
            .field("behavior", &self.behavior)
            .field("call_count", &self.call_count)
            .field("has_server_tokens", &self.server_tokens.is_some())
            .field("has_server", &self.server.is_some())
            .finish()
    }
}

impl FakeActivityGovernorServer {
    pub fn new(behavior: FakeWakeLeaseBehavior) -> Self {
        Self {
            behavior,
            call_count: Arc::new(AtomicUsize::new(0)),
            server_tokens: None,
            server: None,
        }
    }

    /// Shares a counter incremented on every wake lease acquisition call.
    pub fn with_call_count(mut self, call_count: Arc<AtomicUsize>) -> Self {
        self.call_count = call_count;
        self
    }

    /// Forwards the `server_token` of each granted lease to `server_tokens`.
    pub fn with_server_tokens(
        mut self,
        server_tokens: mpsc::UnboundedSender<zx::EventPair>,
    ) -> Self {
        self.server_tokens = Some(server_tokens);
        self
    }

    /// Provides the server handle required by [`FakeWakeLeaseBehavior::CloseWithEpitaph`].
    ///
    /// # Panics
    ///
    /// If [`FakeWakeLeaseBehavior::CloseWithEpitaph`] is used without calling `with_server`,
    /// handling a wake lease request will panic.
    pub fn with_server(mut self, server: fidl_next::Server<fsystem::ActivityGovernor>) -> Self {
        self.server = Some(server);
        self
    }

    fn record_server_token(&self, server_token: zx::EventPair) {
        if let Some(tokens) = &self.server_tokens {
            let _ = tokens.unbounded_send(server_token);
        }
    }

    fn grant_wake_lease(
        &self,
        name: &str,
    ) -> Result<zx::EventPair, fsystem::AcquireWakeLeaseError> {
        if name.is_empty() {
            return Err(fsystem::AcquireWakeLeaseError::InvalidName);
        }
        let (client_token, server_token) = zx::EventPair::create();
        let client_token = client_token
            .replace_handle(zx::Rights::TRANSFER | zx::Rights::DUPLICATE | zx::Rights::WAIT)
            .map_err(|_| fsystem::AcquireWakeLeaseError::Internal)?;
        self.grant_wake_lease_with_token(name, server_token)?;
        Ok(client_token)
    }

    fn grant_wake_lease_with_token(
        &self,
        name: &str,
        server_token: zx::EventPair,
    ) -> Result<(), fsystem::AcquireWakeLeaseError> {
        if name.is_empty() {
            return Err(fsystem::AcquireWakeLeaseError::InvalidName);
        }
        self.record_server_token(server_token);
        Ok(())
    }

    async fn handle_wake_lease<M, T, R>(
        &self,
        responder: fidl_next::Responder<M, T>,
        grant: impl FnOnce(&Self) -> Result<R, fsystem::AcquireWakeLeaseError>,
    ) where
        T: fidl_next::Transport,
        M: fidl_next::TwoWayMethod
            + fidl_next::Respond<R>
            + fidl_next::RespondErr<fsystem::AcquireWakeLeaseError>,
        M::Response: fidl_next::Wire<Constraint = ()>,
        <M as fidl_next::Respond<R>>::Output: fidl_next::Encode<M::Response, T::SendBuffer>,
        <M as fidl_next::RespondErr<fsystem::AcquireWakeLeaseError>>::Output:
            fidl_next::Encode<M::Response, T::SendBuffer>,
    {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        match self.behavior {
            FakeWakeLeaseBehavior::Grant => match grant(self) {
                Ok(response) => {
                    let _ = responder.respond(response).await;
                }
                Err(err) => {
                    let _ = responder.respond_err(err).await;
                }
            },
            FakeWakeLeaseBehavior::Error(err) => {
                let _ = responder.respond_err(err).await;
            }
            FakeWakeLeaseBehavior::Close => {
                self.close_server(None);
            }
            FakeWakeLeaseBehavior::CloseWithEpitaph(status) => {
                self.close_server(Some(status));
            }
        }
    }

    fn close_server(&self, epitaph: Option<zx::Status>) {
        match epitaph {
            None => {
                // Note: If `self.server` is `None`, dropping `responder` without responding
                // also closes the channel automatically in `fidl_next`.
                if let Some(server) = &self.server {
                    server.close();
                }
            }
            Some(status) => {
                self.server
                    .as_ref()
                    .expect("CloseWithEpitaph requires with_server()")
                    .close_with_epitaph(status.into_raw());
            }
        }
    }
}

impl ActivityGovernorTestServerHandler for FakeActivityGovernorServer {
    async fn acquire_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLease>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLease>,
    ) {
        self.handle_wake_lease(responder, |this| this.grant_wake_lease(&request.payload().name))
            .await;
    }

    async fn acquire_unmonitored_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireUnmonitoredWakeLease>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireUnmonitoredWakeLease>,
    ) {
        self.handle_wake_lease(responder, |this| this.grant_wake_lease(&request.payload().name))
            .await;
    }

    async fn acquire_wake_lease_with_token(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLeaseWithToken>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLeaseWithToken>,
    ) {
        let payload = request.payload();
        self.handle_wake_lease(responder, |this| {
            this.grant_wake_lease_with_token(&payload.name, payload.server_token)
        })
        .await;
    }
}

impl fsystem::ActivityGovernorServerHandler for FakeActivityGovernorServer {
    fn get_power_elements(
        &mut self,
        responder: fidl_next::Responder<fsystem::activity_governor::GetPowerElements>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::get_power_elements(self, responder)
    }

    fn acquire_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLease>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLease>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::acquire_wake_lease(self, request, responder)
    }

    fn acquire_unmonitored_wake_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireUnmonitoredWakeLease>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireUnmonitoredWakeLease>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::acquire_unmonitored_wake_lease(self, request, responder)
    }

    fn acquire_wake_lease_with_token(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::AcquireWakeLeaseWithToken>,
        responder: fidl_next::Responder<fsystem::activity_governor::AcquireWakeLeaseWithToken>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::acquire_wake_lease_with_token(self, request, responder)
    }

    fn take_application_activity_lease(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::TakeApplicationActivityLease>,
        responder: fidl_next::Responder<fsystem::activity_governor::TakeApplicationActivityLease>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::take_application_activity_lease(self, request, responder)
    }

    fn register_suspend_blocker(
        &mut self,
        request: fidl_next::Request<fsystem::activity_governor::RegisterSuspendBlocker>,
        responder: fidl_next::Responder<fsystem::activity_governor::RegisterSuspendBlocker>,
    ) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::register_suspend_blocker(self, request, responder)
    }

    fn on_unknown_interaction(&mut self, ordinal: u64) -> impl Future<Output = ()> + Send {
        ActivityGovernorTestServerHandler::on_unknown_interaction(self, ordinal)
    }
}

/// Test-side handle for a fake ActivityGovernor spawned with [`FakeActivityGovernor::spawn`].
#[derive(Debug)]
pub struct FakeActivityGovernor {
    /// Number of wake lease acquisition calls received.
    pub call_count: Arc<AtomicUsize>,
    /// `server_token`s of granted leases, in order.
    pub server_tokens: mpsc::UnboundedReceiver<zx::EventPair>,
}

impl FakeActivityGovernor {
    /// Spawns a fake ActivityGovernor on `scope` and returns a connected client.
    pub fn spawn(
        scope: &fasync::ScopeHandle,
        behavior: FakeWakeLeaseBehavior,
    ) -> (fidl_next::Client<fsystem::ActivityGovernor>, Self) {
        let (client_end, server_end) =
            fidl_next::fuchsia::create_channel::<fsystem::ActivityGovernor>();
        let call_count = Arc::new(AtomicUsize::new(0));
        let (tokens_tx, tokens_rx) = mpsc::unbounded();
        let call_count_clone = call_count.clone();
        server_end.spawn_on_with(
            move |server| {
                FakeActivityGovernorServer::new(behavior)
                    .with_call_count(call_count_clone)
                    .with_server_tokens(tokens_tx)
                    .with_server(server)
            },
            scope,
        );
        (client_end.spawn(), Self { call_count, server_tokens: tokens_rx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    #[fuchsia::test]
    async fn test_grant_wake_lease_methods() {
        let scope = fasync::Scope::new_with_name("test_grant_wake_lease_methods");
        let (client, mut sag) =
            FakeActivityGovernor::spawn(&scope.to_handle(), FakeWakeLeaseBehavior::Grant);

        // 1. AcquireWakeLease
        let lease1 = client
            .acquire_wake_lease("lease-1")
            .await
            .expect("FIDL call should succeed")
            .expect("AcquireWakeLease should succeed")
            .token;
        assert_eq!(
            lease1.basic_info().expect("basic_info").rights,
            zx::Rights::TRANSFER | zx::Rights::DUPLICATE | zx::Rights::WAIT
        );
        let server_token1 = sag.server_tokens.next().await.expect("Expected server_token 1");
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 1);

        // 2. AcquireUnmonitoredWakeLease
        let lease2 = client
            .acquire_unmonitored_wake_lease("lease-2")
            .await
            .expect("FIDL call should succeed")
            .expect("AcquireUnmonitoredWakeLease should succeed")
            .token;
        assert_eq!(
            lease2.basic_info().expect("basic_info").rights,
            zx::Rights::TRANSFER | zx::Rights::DUPLICATE | zx::Rights::WAIT
        );
        let server_token2 = sag.server_tokens.next().await.expect("Expected server_token 2");
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 2);

        // 3. AcquireWakeLeaseWithToken
        let (lease3, token3_server) = zx::EventPair::create();
        client
            .acquire_wake_lease_with_token("lease-3", token3_server)
            .await
            .expect("FIDL call should succeed")
            .expect("AcquireWakeLeaseWithToken should succeed");
        let server_token3 = sag.server_tokens.next().await.expect("Expected server_token 3");
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 3);

        drop(lease1);
        fasync::OnSignals::new(&server_token1, zx::Signals::EVENTPAIR_PEER_CLOSED)
            .await
            .expect("server_token1 should observe PEER_CLOSED");

        drop(lease2);
        fasync::OnSignals::new(&server_token2, zx::Signals::EVENTPAIR_PEER_CLOSED)
            .await
            .expect("server_token2 should observe PEER_CLOSED");

        drop(lease3);
        fasync::OnSignals::new(&server_token3, zx::Signals::EVENTPAIR_PEER_CLOSED)
            .await
            .expect("server_token3 should observe PEER_CLOSED");
    }

    #[fuchsia::test]
    async fn test_wake_lease_validation_errors() {
        let scope = fasync::Scope::new_with_name("test_wake_lease_validation_errors");
        let (client, _sag) =
            FakeActivityGovernor::spawn(&scope.to_handle(), FakeWakeLeaseBehavior::Grant);

        // Empty name across all 3 methods -> InvalidName
        assert_eq!(
            client.acquire_wake_lease("").await.expect("FIDL call"),
            Err(fsystem::AcquireWakeLeaseError::InvalidName)
        );
        assert_eq!(
            client.acquire_unmonitored_wake_lease("").await.expect("FIDL call"),
            Err(fsystem::AcquireWakeLeaseError::InvalidName)
        );
        let (_client_tok, server_tok) = zx::EventPair::create();
        assert_eq!(
            client.acquire_wake_lease_with_token("", server_tok).await.expect("FIDL call"),
            Err(fsystem::AcquireWakeLeaseError::InvalidName)
        );
    }

    #[fuchsia::test]
    async fn test_error_close_and_epitaph_behaviors() {
        let scope = fasync::Scope::new_with_name("test_error_close_and_epitaph_behaviors");

        // 1. Error behavior
        let (client, sag) = FakeActivityGovernor::spawn(
            &scope.to_handle(),
            FakeWakeLeaseBehavior::Error(fsystem::AcquireWakeLeaseError::Internal),
        );
        assert_eq!(
            client.acquire_wake_lease("test").await.expect("FIDL call"),
            Err(fsystem::AcquireWakeLeaseError::Internal)
        );
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 1);

        // 2. Close behavior via FakeActivityGovernor::spawn (with_server)
        let (client, sag) =
            FakeActivityGovernor::spawn(&scope.to_handle(), FakeWakeLeaseBehavior::Close);
        assert!(matches!(
            client.acquire_wake_lease("test").await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed))
        ));
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 1);

        // 3. Close behavior without calling with_server
        let (client_end, server_end) =
            fidl_next::fuchsia::create_channel::<fsystem::ActivityGovernor>();
        server_end.spawn_on(
            FakeActivityGovernorServer::new(FakeWakeLeaseBehavior::Close),
            &scope.to_handle(),
        );
        let client = client_end.spawn();
        assert!(matches!(
            client.acquire_wake_lease("test").await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed))
        ));

        // 4. CloseWithEpitaph behavior
        let (client, sag) = FakeActivityGovernor::spawn(
            &scope.to_handle(),
            FakeWakeLeaseBehavior::CloseWithEpitaph(zx::Status::NOT_FOUND),
        );
        assert!(matches!(
            client.acquire_wake_lease("test").await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(
                epitaph
            ))) if epitaph == zx::Status::NOT_FOUND.into_raw()
        ));
        assert_eq!(sag.call_count.load(Ordering::SeqCst), 1);
    }

    #[fuchsia::test]
    async fn test_custom_handler_via_test_server_trait() {
        struct CustomGovernor {
            get_power_elements_called: Arc<AtomicUsize>,
            not_implemented_calls: Arc<AtomicUsize>,
        }

        impl ActivityGovernorTestServerHandler for CustomGovernor {
            fn not_implemented(&mut self, _method_name: &str) {
                self.not_implemented_calls.fetch_add(1, Ordering::SeqCst);
            }

            async fn get_power_elements(
                &mut self,
                responder: fidl_next::Responder<fsystem::activity_governor::GetPowerElements>,
            ) {
                self.get_power_elements_called.fetch_add(1, Ordering::SeqCst);
                let _ = responder.respond(fsystem::PowerElements::default()).await;
            }
        }

        let scope = fasync::Scope::new_with_name("test_custom_handler");
        let (client_end, server_end) =
            fidl_next::fuchsia::create_channel::<fsystem::ActivityGovernor>();
        let called = Arc::new(AtomicUsize::new(0));
        let not_implemented_calls = Arc::new(AtomicUsize::new(0));
        server_end.spawn_on(
            ActivityGovernorTestServer(CustomGovernor {
                get_power_elements_called: called.clone(),
                not_implemented_calls: not_implemented_calls.clone(),
            }),
            &scope.to_handle(),
        );
        let client = client_end.spawn();

        let _ = client.get_power_elements().await.expect("get_power_elements should succeed");
        assert_eq!(called.load(Ordering::SeqCst), 1);

        // Calling an unoverridden method invokes `not_implemented` and drops the responder,
        // closing the channel.
        assert!(matches!(
            client.take_application_activity_lease("app").await,
            Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed))
        ));
        assert_eq!(not_implemented_calls.load(Ordering::SeqCst), 1);
    }
}
