// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::RoutingError;
use async_trait::async_trait;
use capability_source::{CapabilitySource, RemotedAtSource};
use cm_rust::CapabilityTypeName;
use cm_types::{IterablePath, RelativePath};
use fidl_fuchsia_component_runtime::RouteRequest;
use itertools::Itertools;
use moniker::ExtendedMoniker;
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Dictionary, Routable, Router, WeakInstanceToken,
};
use std::fmt::Debug;
use std::sync::Arc;

#[async_trait]
pub trait DictExt {
    /// Returns the capability at the path, if it exists. Returns `None` if path is empty.
    fn get_capability(&self, path: &impl IterablePath) -> Option<Capability>;

    /// Looks up a top-level router in this [Dictionary] with return type `T`. If it's not found
    /// (or it's not a router) returns a router that always returns `not_found_error`. If `path`
    /// has one segment and a router was found, returns that router.
    ///
    /// If `path` is a multi-segment path, the returned router performs a [Dictionary] lookup with
    /// the remaining path relative to the top-level router (see [LazyGet::lazy_get]).
    ///
    /// REQUIRES: `path` is not empty.
    fn get_router_or_not_found<T>(
        &self,
        path: &impl IterablePath,
        not_found_error: RoutingError,
    ) -> Arc<Router<T>>
    where
        T: CapabilityBound,
        Arc<T>: TryFrom<Capability>,
        Arc<Router<T>>: TryFrom<Capability>,
        Capability: From<Arc<T>>,
        Capability: From<Arc<Router<T>>>,
        Router<T>: CapabilityBound;

    /// Inserts the capability at the path. Intermediary dictionaries are created as needed. If
    /// there's already a capability at the path, then the preexisting value is returned.
    fn insert_capability(
        &self,
        path: &impl IterablePath,
        capability: Capability,
    ) -> Option<Capability>;

    /// Removes the capability at the path, if it exists, and returns it.
    fn remove_capability(&self, path: &impl IterablePath) -> Option<Capability>;

    /// Looks up the element at `path`. When encountering an intermediate router, use `request` to
    /// request the underlying capability from it. In contrast, `get_capability` will return
    /// `None`.
    ///
    /// Note that the return value can contain any capability type, instead of a parameterized `T`.
    /// This is because some callers work with a generic capability and don't care about the
    /// specific type.
    async fn get_with_request<'a>(
        &self,
        moniker: &ExtendedMoniker,
        path: &'a impl IterablePath,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Capability>, RouterError>;

    /// Identical to `get_with_request`, except it returns the source of the capability at `path`
    /// instead of the capability itself.
    async fn get_with_request_debug<'a>(
        &self,
        moniker: &ExtendedMoniker,
        path: &'a impl IterablePath,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError>;
}

#[async_trait]
impl DictExt for Arc<Dictionary> {
    fn get_capability(&self, path: &impl IterablePath) -> Option<Capability> {
        let mut segments = path.iter_segments();
        let Some(mut current_name) = segments.next() else {
            return Some(Capability::Dictionary(self.clone()));
        };
        let mut current_dict = self.clone();
        loop {
            match segments.next() {
                Some(next_name) => {
                    let sub_dict =
                        current_dict.get(current_name).and_then(|value| value.to_dictionary())?;
                    current_dict = sub_dict;

                    current_name = next_name;
                }
                None => return current_dict.get(current_name),
            }
        }
    }

    fn get_router_or_not_found<T>(
        &self,
        path: &impl IterablePath,
        not_found_error: RoutingError,
    ) -> Arc<Router<T>>
    where
        T: CapabilityBound,
        Arc<T>: TryFrom<Capability>,
        Arc<Router<T>>: TryFrom<Capability>,
        Router<T>: CapabilityBound,
        Capability: From<Arc<T>>,
        Capability: From<Arc<Router<T>>>,
    {
        let mut segments = path.iter_segments();
        let root = segments.next().expect("path must be nonempty");

        #[derive(Debug)]
        struct ErrorRouter {
            not_found_error: RouterError,
        }

        #[async_trait]
        impl<T: CapabilityBound> Routable<T> for ErrorRouter {
            async fn route(
                &self,
                _request: RouteRequest,
                _target: Arc<WeakInstanceToken>,
            ) -> Result<Option<Arc<T>>, RouterError> {
                Err(self.not_found_error.clone())
            }

            async fn route_debug(
                &self,
                _request: RouteRequest,
                _target: Arc<WeakInstanceToken>,
            ) -> Result<CapabilitySource, RouterError> {
                Err(self.not_found_error.clone())
            }
        }

        /// This uses the same algorithm as [LazyGet], but that is implemented for
        /// [Router<Dictionary>] while this is implemented for [Router]. This duplication will go
        /// away once [Router] is replaced with [Router].
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
            Arc<Router<T>>: TryFrom<Capability>,
            Capability: From<Arc<Router<T>>>,
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

        if segments.next().is_none() {
            // No nested lookup necessary.
            let Some(router) = self.get(root).and_then(|cap| cap.try_into().ok()) else {
                return Router::<T>::new(ErrorRouter { not_found_error: not_found_error.into() });
            };
            return router;
        }

        let Some(cap) = self.get(root) else {
            return Router::<T>::new(ErrorRouter { not_found_error: not_found_error.into() });
        };
        let router = match cap {
            Capability::Dictionary(d) => Router::<Dictionary>::new_ok(d),
            Capability::DictionaryRouter(r) => r,
            _ => {
                return Router::<T>::new(ErrorRouter { not_found_error: not_found_error.into() });
            }
        };

        let mut segments = path.iter_segments();
        let _ = segments.next().unwrap();
        let path = RelativePath::from(segments.collect::<Vec<_>>());

        Router::<T>::new(ScopedDictRouter { router, path, not_found_error: not_found_error.into() })
    }

    fn insert_capability(
        &self,
        path: &impl IterablePath,
        capability: Capability,
    ) -> Option<Capability> {
        let mut segments = path.iter_segments();
        let mut current_name = segments.next().expect("path must be non-empty");
        let mut current_dict = self.clone();
        loop {
            match segments.next() {
                Some(next_name) => {
                    let sub_dict = {
                        match current_dict.get(current_name) {
                            Some(Capability::Dictionary(dict)) => dict,
                            Some(Capability::DictionaryRouter(preexisting_router)) => {
                                let mut path = vec![next_name];
                                while let Some(name) = segments.next() {
                                    path.push(name);
                                }
                                let path = RelativePath::from(path);
                                let new_router = Router::new(AdditiveDictionaryRouter {
                                    preexisting_router,
                                    path,
                                    capability,
                                });

                                // Replace the entry in current_dict.
                                return current_dict.insert(current_name.into(), new_router.into());
                            }
                            None => {
                                let dict = Dictionary::new();
                                current_dict.insert(
                                    current_name.into(),
                                    Capability::Dictionary(dict.clone()),
                                );
                                dict
                            }
                            _ => return None,
                        }
                    };
                    current_dict = sub_dict;

                    current_name = next_name;
                }
                None => {
                    return current_dict.insert(current_name.into(), capability);
                }
            }
        }
    }

    fn remove_capability(&self, path: &impl IterablePath) -> Option<Capability> {
        let mut segments = path.iter_segments();
        let mut current_name = segments.next().expect("path must be non-empty");
        let mut current_dict = self.clone();
        loop {
            match segments.next() {
                Some(next_name) => {
                    let sub_dict =
                        current_dict.get(current_name).and_then(|value| value.to_dictionary());
                    if sub_dict.is_none() {
                        // The capability doesn't exist, there's nothing to remove.
                        return None;
                    }
                    current_dict = sub_dict.unwrap();
                    current_name = next_name;
                }
                None => {
                    return current_dict.remove(current_name);
                }
            }
        }
    }

    async fn get_with_request<'a>(
        &self,
        moniker: &ExtendedMoniker,
        path: &'a impl IterablePath,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Capability>, RouterError> {
        let mut current_dict = self.clone();
        let num_segments = path.iter_segments().count();
        for (next_idx, next_name) in path.iter_segments().enumerate() {
            // Get the capability.
            let capability = current_dict.get(next_name);

            // The capability doesn't exist.
            let Some(capability) = capability else {
                return Err(RoutingError::BedrockNotPresentInDictionary {
                    name: path.iter_segments().join("/"),
                    moniker: moniker.clone(),
                }
                .into());
            };

            if next_idx < num_segments - 1 {
                // Not at the end of the path yet, so there's more nesting. We expect to have found
                // a [Dictionary], or a [Dictionary] router -- traverse into this [Dictionary].
                match capability {
                    Capability::Dictionary(d) => {
                        current_dict = d;
                    }
                    Capability::DictionaryRouter(r) => {
                        let request = request_with_dictionary_replacement(&request)?;
                        let Some(new_dictionary) = r.route(request, target.clone()).await? else {
                            return Ok(None);
                        };
                        current_dict = new_dictionary;
                    }
                    _ => {
                        return Err(RoutingError::BedrockWrongCapabilityType {
                            expected: Dictionary::debug_typename().into(),
                            actual: capability.debug_typename().into(),
                            moniker: moniker.clone(),
                        }
                        .into());
                    }
                }
                continue;
            }

            // We've reached the end of our path. The last capability should have type
            // `T` or `Router<T>`.
            //
            // There's a bit of repetition here because this function supports multiple router
            // types.
            match capability {
                Capability::DictionaryRouter(r) => {
                    return r.route(request, target).await.map(|option| option.map(Into::into));
                }
                Capability::ConnectorRouter(r) => {
                    return r.route(request, target).await.map(|option| option.map(Into::into));
                }
                Capability::DataRouter(r) => {
                    return r.route(request, target).await.map(|option| option.map(Into::into));
                }
                Capability::DirConnectorRouter(r) => {
                    return r.route(request, target).await.map(|option| option.map(Into::into));
                }
                other_capability => return Ok(Some(other_capability.into())),
            };
        }
        unreachable!("get_with_request: All cases are handled in the loop");
    }

    async fn get_with_request_debug<'a>(
        &self,
        moniker: &ExtendedMoniker,
        path: &'a impl IterablePath,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        let mut current_dict = self.clone();
        let mut closest_moniker = moniker.clone();
        let num_segments = path.iter_segments().count();
        for (next_idx, next_name) in path.iter_segments().enumerate() {
            // Get the capability.
            let capability = current_dict.get(next_name);

            // The capability doesn't exist.
            let Some(capability) = capability else {
                return Err(RoutingError::BedrockNotPresentInDictionary {
                    name: path.iter_segments().join("/"),
                    moniker: moniker.clone(),
                }
                .into());
            };

            if next_idx < num_segments - 1 {
                // Not at the end of the path yet, so there's more nesting. We expect to have found
                // a [Dictionary], or a [Dictionary] router -- traverse into this [Dictionary].
                match capability {
                    Capability::Dictionary(d) => {
                        current_dict = d;
                    }
                    Capability::DictionaryRouter(r) => {
                        // We want to do two routes of this: one debug and one non-debug. The debug
                        // route is needed so we can determine where this dictionary comes from
                        // (which is needed below if we find a non-router capability), and the
                        // non-debug route here is needed to recurse into.
                        let req = request_with_dictionary_replacement(&request)?;
                        let maybe_new_dictionary = r.route(req.clone(), target.clone()).await?;
                        let source = r.route_debug(req.clone(), target.clone()).await?;

                        let Some(new_dictionary) = maybe_new_dictionary else {
                            // The capability is not available! Let's return the source of it
                            // (which ought to be [`CapabilitySource::Void`])
                            return Ok(source);
                        };
                        current_dict = new_dictionary;
                        closest_moniker = source.source_moniker();
                    }
                    _ => {
                        return Err(RoutingError::BedrockWrongCapabilityType {
                            expected: Dictionary::debug_typename().into(),
                            actual: capability.debug_typename().into(),
                            moniker: moniker.clone(),
                        }
                        .into());
                    }
                }
                continue;
            }

            // We've reached the end of our path. The last capability should have type
            // `T` or `Router<T>`.
            //
            // There's a bit of repetition here because this function supports multiple router
            // types.
            match capability {
                Capability::DictionaryRouter(r) => {
                    return r.route_debug(request, target).await;
                }
                Capability::ConnectorRouter(r) => {
                    return r.route_debug(request, target).await;
                }
                Capability::DataRouter(r) => {
                    return r.route_debug(request, target).await;
                }
                Capability::DirConnectorRouter(r) => {
                    return r.route_debug(request, target).await;
                }
                _other_capability => {
                    // This is a debug route, and we've found a non-router capability. We must
                    // return debug information for the debug route, and the only reason there
                    // would be a non-router capability in a dictionary would be if a user
                    // created one, so we can safely report that this was a remotely created
                    // capability.
                    //
                    // We attribute this to the provider of the most recent dictionary we routed,
                    // which should be the component that put this non-router capability in a
                    // dictionary.
                    let remoted_at_moniker = match closest_moniker {
                        ExtendedMoniker::ComponentInstance(m) => m,
                        // Component manager always generates routers, so we should never find
                        // a non-router capability at the point where this moniker would be for
                        // component manager.
                        ExtendedMoniker::ComponentManager => {
                            panic!("component manager generated a non-router capability")
                        }
                    };
                    let type_name: Option<CapabilityTypeName> = request
                        .build_type_name
                        .as_ref()
                        .map(|s| std::str::FromStr::from_str(s.as_str()))
                        .transpose()
                        .expect("invalid type name");
                    return Ok(CapabilitySource::RemotedAt(RemotedAtSource {
                        moniker: remoted_at_moniker,
                        type_name,
                    }));
                }
            };
        }
        unreachable!("get_with_request_debug: All cases are handled in the loop");
    }
}

/// Creates a clone of `request` that is identical except `"type"` is set to `dictionary`
/// if it is not already. If `request` is `None`, `None` will be returned.
///
/// This is convenient for router lookups of nested paths, since all lookups except the last
/// segment are dictionary lookups.
pub(super) fn request_with_dictionary_replacement(
    request: &RouteRequest,
) -> Result<RouteRequest, RoutingError> {
    if request == &RouteRequest::default() {
        return Ok(RouteRequest::default());
    }
    let mut request_clone = request.clone();
    request_clone.build_type_name = Some(CapabilityTypeName::Dictionary.to_string());
    Ok(request_clone)
}

struct AdditiveDictionaryRouter {
    preexisting_router: Arc<Router<Dictionary>>,
    path: RelativePath,
    capability: Capability,
}

#[async_trait]
impl Routable<Dictionary> for AdditiveDictionaryRouter {
    async fn route(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<Dictionary>>, RouterError> {
        let dictionary = match self.preexisting_router.route(request, target).await {
            Ok(Some(dictionary)) => dictionary.shallow_copy(),
            other_response => return other_response,
        };
        let _ = dictionary.insert_capability(&self.path, self.capability.clone());
        Ok(Some(dictionary))
    }

    async fn route_debug(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        self.preexisting_router.route_debug(request, target).await
    }
}
