// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{CapabilityBound, WeakInstanceToken};
use async_trait::async_trait;
use capability_source::CapabilitySource;
use fidl_fuchsia_component_runtime::RouteRequest;
use futures::FutureExt;
use futures::future::BoxFuture;
use router_error::RouterError;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;

/// Response of a [Router] request.
#[derive(Debug)]
pub enum RouterResponse<T: CapabilityBound> {
    /// Routing succeeded and returned this capability.
    Capability(T),

    /// Routing succeeded, but the capability was marked unavailable.
    Unavailable,

    /// Routing succeeded in debug mode.
    Debug(Box<CapabilitySource>),
}

/// Types that implement [`Routable`] let the holder asynchronously request capabilities
/// from them.
#[async_trait]
pub trait Routable<T>: Send + Sync
where
    T: CapabilityBound,
{
    async fn route(
        &self,
        request: RouteRequest,
        // A reference to the requesting component.
        target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError>;

    /// Performs the same operation as `route`, but returns a
    /// `fidl_fuchsia_internal::CapabilitySource` persisted into bytes.
    async fn route_debug(
        &self,
        request: RouteRequest,
        // A reference to the requesting component.
        target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError>;
}

/// A [`Router`] is a capability that lets the holder obtain other capabilities
/// asynchronously. [`Router`] is the object capability representation of
/// [`Routable`].
///
/// During routing, a request usually traverses through the component topology,
/// passing through several routers, ending up at some router that will fulfill
/// the request instead of forwarding it upstream.
///
/// [`Router`] differs from [`Router`] in that it is parameterized on the capability
/// type `T`. Instead of a [`Capability`], [`Router`] returns a [`RouterResponse`].
/// [`Router`] will supersede [`Router`].
#[derive(Clone)]
pub struct Router<T: CapabilityBound> {
    routable: Arc<dyn Routable<T>>,
}

impl CapabilityBound for Router<crate::Connector> {
    fn debug_typename() -> &'static str {
        "ConnectorRouter"
    }
}
impl CapabilityBound for Router<crate::Data> {
    fn debug_typename() -> &'static str {
        "DataRouter"
    }
}
impl CapabilityBound for Router<crate::Dictionary> {
    fn debug_typename() -> &'static str {
        "DictionaryRouter"
    }
}

impl CapabilityBound for Router<crate::DirConnector> {
    fn debug_typename() -> &'static str {
        "DirConnectorRouter"
    }
}

impl<T: CapabilityBound> fmt::Debug for Router<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO(https://fxbug.dev/329680070): Require `Debug` on `Routable` trait.
        f.debug_struct("Router").field("routable", &"[some routable object]").finish()
    }
}

/// Syntax sugar within the framework to express custom routing logic using a function
/// that takes a request and returns such future.
impl<T: CapabilityBound, F> Routable<T> for F
where
    F: Fn(
            RouteRequest,
            bool,
            WeakInstanceToken,
        ) -> BoxFuture<'static, Result<RouterResponse<T>, RouterError>>
        + Send
        + Sync
        + 'static,
{
    // We use the desugared form of `async_trait` to avoid unnecessary boxing.
    fn route<'a, 'b>(
        &'a self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> BoxFuture<'b, Result<Option<T>, RouterError>>
    where
        'a: 'b,
        Self: 'b,
    {
        async move {
            match self(request, false, target).await? {
                RouterResponse::Capability(c) => Ok(Some(c)),
                RouterResponse::Unavailable => Ok(None),
                RouterResponse::Debug(_) => {
                    panic!("router returned debug info for non-debug route")
                }
            }
        }
        .boxed()
    }

    fn route_debug<'a, 'b>(
        &'a self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> BoxFuture<'b, Result<CapabilitySource, RouterError>>
    where
        'a: 'b,
        Self: 'b,
    {
        async move {
            match self(request, true, target).await? {
                RouterResponse::Capability(_) | RouterResponse::Unavailable => {
                    panic!("router returned non-debug info for debug route")
                }
                RouterResponse::Debug(source) => Ok(*source),
            }
        }
        .boxed()
    }
}

#[async_trait]
impl<T: CapabilityBound> Routable<T> for Router<T> {
    async fn route(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError> {
        Router::route(self, request, target).await
    }

    async fn route_debug(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        Router::route_debug(self, request, target).await
    }
}

impl<T: CapabilityBound> Router<T> {
    /// Package a [`Routable`] object into a [`Router`].
    pub fn new(routable: impl Routable<T> + 'static) -> Self {
        Self { routable: Arc::new(routable) }
    }

    /// Creates a router that will always fail a request with the provided error.
    pub fn new_error(error: impl Into<RouterError>) -> Self {
        let v: RouterError = error.into();
        Self::new(ErrRouter { v })
    }

    /// Creates a router that will always return the given debug info.
    pub fn new_debug(source: CapabilitySource) -> Self {
        Self::new(DebugRouter { source })
    }

    /// Obtain a capability from this router, following the description in `request`.
    pub async fn route(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError> {
        self.routable.route(request, target).await
    }

    /// Obtain a CapabilitySource from this router, following the description in `request`.
    pub async fn route_debug(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        self.routable.route_debug(request, target).await
    }
}

impl<T: Clone + CapabilityBound> Router<T> {
    /// Creates a router that will always resolve with the provided capability.
    // TODO: Should this require debug info?
    pub fn new_ok(c: impl Into<T>) -> Self {
        let v: T = c.into();
        Self::new(OkRouter { v })
    }
}

#[derive(Clone)]
struct OkRouter<T: Clone + CapabilityBound> {
    v: T,
}

#[async_trait]
impl<T: Clone + CapabilityBound> Routable<T> for OkRouter<T> {
    async fn route(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError> {
        Ok(Some(self.v.clone()))
    }

    async fn route_debug(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        panic!("OkRouter does not handle debug routes");
    }
}

#[derive(Clone)]
struct DebugRouter {
    source: CapabilitySource,
}

#[async_trait]
impl<T: CapabilityBound> Routable<T> for DebugRouter {
    async fn route(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError> {
        panic!("DebugRouter does not handle non-debug routes");
    }

    async fn route_debug(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        Ok(self.source.clone())
    }
}

#[derive(Clone)]
struct ErrRouter {
    v: RouterError,
}

#[async_trait]
impl<T: CapabilityBound> Routable<T> for ErrRouter {
    async fn route(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<Option<T>, RouterError> {
        Err(self.v.clone())
    }

    async fn route_debug(
        &self,
        _request: RouteRequest,
        _target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        Err(self.v.clone())
    }
}
