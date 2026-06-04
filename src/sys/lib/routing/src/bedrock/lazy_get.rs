// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bedrock::dict_ext::request_with_dictionary_replacement;
use crate::{DictExt, RoutingError};
use async_trait::async_trait;
use capability_source::CapabilitySource;
use cm_types::IterablePath;
use fidl_fuchsia_component_runtime::RouteRequest;
use moniker::ExtendedMoniker;
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Dictionary, Routable, Router, WeakInstanceToken,
};
use std::fmt::Debug;
use std::sync::Arc;

/// Implements the `lazy_get` function for [`Routable<Dictionary>`].
pub trait LazyGet<T: CapabilityBound> {
    /// Returns a router that requests a dictionary from the specified `path` relative to
    /// the base routable or fails the request with `not_found_error` if the member is not
    /// found.
    fn lazy_get<P>(self, path: P, not_found_error: RoutingError) -> Arc<Router<T>>
    where
        P: IterablePath + Debug + 'static;
}

impl<T: CapabilityBound> LazyGet<T> for Arc<Router<Dictionary>>
where
    Arc<T>: TryFrom<Capability>,
{
    fn lazy_get<P>(self, path: P, not_found_error: RoutingError) -> Arc<Router<T>>
    where
        P: IterablePath + Debug + 'static,
    {
        #[derive(Debug)]
        struct ScopedDictRouter<P: IterablePath + Debug + 'static> {
            router: Arc<Router<Dictionary>>,
            path: P,
            not_found_error: RoutingError,
        }

        #[async_trait]
        impl<P: IterablePath + Debug + 'static, T: CapabilityBound> Routable<T> for ScopedDictRouter<P>
        where
            Arc<T>: TryFrom<Capability>,
        {
            async fn route(
                &self,
                request: RouteRequest,
                target: Arc<WeakInstanceToken>,
            ) -> Result<Option<Arc<T>>, RouterError> {
                let get_init_request = || request_with_dictionary_replacement(&request);

                let init_request = (get_init_request)()?;
                match self.router.route(init_request, target.clone()).await? {
                    Some(dict) => {
                        let moniker: ExtendedMoniker = self.not_found_error.clone().into();
                        match dict.get_with_request(&moniker, &self.path, request, target).await {
                            Err(router_error)
                                if let Ok(RoutingError::BedrockNotPresentInDictionary {
                                    ..
                                }) = router_error.clone().try_into() =>
                            {
                                Err(self.not_found_error.clone().into())
                            }
                            Err(e) => Err(e),
                            Ok(None) => Ok(None),
                            Ok(Some(cap)) => {
                                let actual_type_name = cap.debug_typename();
                                let cap: Arc<T> = cap.try_into().map_err(|_| {
                                    RoutingError::BedrockWrongCapabilityType {
                                        expected: T::debug_typename().into(),
                                        actual: actual_type_name.into(),
                                        moniker,
                                    }
                                })?;
                                Ok(Some(cap))
                            }
                        }
                    }
                    None => Ok(None),
                }
            }

            async fn route_debug(
                &self,
                request: RouteRequest,
                target: Arc<WeakInstanceToken>,
            ) -> Result<CapabilitySource, RouterError> {
                let get_init_request = || request_with_dictionary_replacement(&request);

                // When performing a debug route, we only want to call `route_debug` on the
                // capability at `path`. Here we're looking up the containing dictionary, so we do
                // non-debug routing, to obtain the actual Dictionary and not its debug info.
                let init_request = (get_init_request)()?;
                match self.router.route(init_request, target.clone()).await? {
                    Some(dict) => {
                        let moniker: ExtendedMoniker = self.not_found_error.clone().into();
                        match dict
                            .get_with_request_debug(&moniker, &self.path, request, target)
                            .await
                        {
                            Err(router_error)
                                if let Ok(RoutingError::BedrockNotPresentInDictionary {
                                    ..
                                }) = router_error.clone().try_into() =>
                            {
                                Err(self.not_found_error.clone().into())
                            }
                            other_result => other_result,
                        }
                    }
                    None => {
                        // The above route was non-debug, but the routing operation failed. Call
                        // the router again with the same arguments but with `route_debug` so that
                        // we return the debug info to the caller (which ought to be
                        // [`CapabilitySource::Void`]).
                        let init_request = (get_init_request)()?;
                        self.router.route_debug(init_request, target).await
                    }
                }
            }
        }

        Router::<T>::new(ScopedDictRouter {
            router: self,
            path,
            not_found_error: not_found_error.into(),
        })
    }
}
