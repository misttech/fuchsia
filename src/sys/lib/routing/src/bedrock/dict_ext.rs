// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::RoutingError;
use async_trait::async_trait;
use capability_source::{CapabilitySource, RemotedAtSource};
use cm_rust::CapabilityTypeName;
use cm_types::{IterablePath, RelativePath};
use fidl_fuchsia_component_runtime::RouteRequest;
use fidl_fuchsia_component_sandbox as fsandbox;
use moniker::ExtendedMoniker;
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Dictionary, Routable, Router, RouterResponse, WeakInstanceToken,
};
use std::fmt::Debug;

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
    ) -> Router<T>
    where
        T: CapabilityBound,
        Router<T>: TryFrom<Capability>;

    /// Inserts the capability at the path. Intermediary dictionaries are created as needed.
    fn insert_capability(
        &self,
        path: &impl IterablePath,
        capability: Capability,
    ) -> Result<(), fsandbox::CapabilityStoreError>;

    /// Removes the capability at the path, if it exists, and returns it.
    fn remove_capability(&self, path: &impl IterablePath) -> Option<Capability>;

    /// Looks up the element at `path`. When encountering an intermediate router, use `request` to
    /// request the underlying capability from it. In contrast, `get_capability` will return
    /// `None`.
    ///
    /// Note that the return value can contain any capability type, instead of a parameterized `T`.
    /// This is because some callers work with a generic capability and don't care about the
    /// specific type. Callers who do care can use `TryFrom` to cast to the expected
    /// [RouterResponse] type.
    async fn get_with_request<'a>(
        &self,
        moniker: &ExtendedMoniker,
        path: &'a impl IterablePath,
        request: RouteRequest,
        debug: bool,
        target: WeakInstanceToken,
    ) -> Result<Option<GenericRouterResponse>, RouterError>;
}

/// The analogue of a [RouterResponse] that can hold any type of capability. This is the
/// return type of [DictExt::get_with_request].
#[derive(Debug)]
pub enum GenericRouterResponse {
    /// Routing succeeded and returned this capability.
    Capability(Capability),

    /// Routing succeeded, but the capability was marked unavailable.
    Unavailable,

    /// Routing succeeded in debug mode, `Data` contains the debug data.
    Debug(Box<CapabilitySource>),
}

impl<T: CapabilityBound> TryFrom<GenericRouterResponse> for RouterResponse<T> {
    // Returns the capability's debug typename.
    type Error = &'static str;

    fn try_from(r: GenericRouterResponse) -> Result<Self, Self::Error> {
        let r = match r {
            GenericRouterResponse::Capability(c) => {
                let debug_name = c.debug_typename();
                RouterResponse::<T>::Capability(c.try_into().map_err(|_| debug_name)?)
            }
            GenericRouterResponse::Unavailable => RouterResponse::<T>::Unavailable,
            GenericRouterResponse::Debug(d) => RouterResponse::<T>::Debug(d),
        };
        Ok(r)
    }
}

impl<T: CapabilityBound> TryFrom<GenericRouterResponse> for Option<T> {
    // Returns the capability's debug typename.
    type Error = &'static str;

    fn try_from(r: GenericRouterResponse) -> Result<Self, Self::Error> {
        let r = match r {
            GenericRouterResponse::Capability(c) => {
                let debug_name = c.debug_typename();
                Some(c.try_into().map_err(|_| debug_name)?)
            }
            GenericRouterResponse::Unavailable => None,
            GenericRouterResponse::Debug(_) => return Err("unexpected debug value"),
        };
        Ok(r)
    }
}

#[async_trait]
impl DictExt for Dictionary {
    fn get_capability(&self, path: &impl IterablePath) -> Option<Capability> {
        let mut segments = path.iter_segments();
        let Some(mut current_name) = segments.next() else { return Some(self.clone().into()) };
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
    ) -> Router<T>
    where
        T: CapabilityBound,
        Router<T>: TryFrom<Capability>,
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
                _target: WeakInstanceToken,
            ) -> Result<Option<T>, RouterError> {
                Err(self.not_found_error.clone())
            }

            async fn route_debug(
                &self,
                _request: RouteRequest,
                _target: WeakInstanceToken,
            ) -> Result<CapabilitySource, RouterError> {
                Err(self.not_found_error.clone())
            }
        }

        /// This uses the same algorithm as [LazyGet], but that is implemented for
        /// [Router<Dictionary>] while this is implemented for [Router]. This duplication will go
        /// away once [Router] is replaced with [Router].
        #[derive(Debug)]
        struct ScopedDictRouter<P: IterablePath + Debug + 'static> {
            router: Router<Dictionary>,
            path: P,
            not_found_error: RoutingError,
        }

        #[async_trait]
        impl<P: IterablePath + Debug + 'static, T: CapabilityBound> Routable<T> for ScopedDictRouter<P> {
            async fn route(
                &self,
                request: RouteRequest,
                target: WeakInstanceToken,
            ) -> Result<Option<T>, RouterError> {
                let get_init_request = || request_with_dictionary_replacement(&request);

                let init_request = (get_init_request)()?;
                match self.router.route(init_request, target.clone()).await? {
                    Some(dict) => {
                        let moniker: ExtendedMoniker = self.not_found_error.clone().into();
                        let resp = dict
                            .get_with_request(&moniker, &self.path, request, false, target)
                            .await?;
                        let resp =
                            resp.ok_or_else(|| RouterError::from(self.not_found_error.clone()))?;
                        let resp = resp.try_into().map_err(|debug_name: &'static str| {
                            RoutingError::BedrockWrongCapabilityType {
                                expected: T::debug_typename().into(),
                                actual: debug_name.into(),
                                moniker,
                            }
                        })?;
                        Ok(resp)
                    }
                    None => Ok(None),
                }
            }

            async fn route_debug(
                &self,
                request: RouteRequest,
                target: WeakInstanceToken,
            ) -> Result<CapabilitySource, RouterError> {
                let get_init_request = || request_with_dictionary_replacement(&request);

                // When performing a debug route, we only want to call `route_debug` on the
                // capability at `path`. Here we're looking up the containing dictionary, so we do
                // non-debug routing, to obtain the actual Dictionary and not its debug info.
                let init_request = (get_init_request)()?;
                match self.router.route(init_request, target.clone()).await? {
                    Some(dict) => {
                        let moniker: ExtendedMoniker = self.not_found_error.clone().into();
                        let resp = dict
                            .get_with_request(&moniker, &self.path, request, true, target)
                            .await?;
                        let resp =
                            resp.ok_or_else(|| RouterError::from(self.not_found_error.clone()))?;
                        match resp {
                            GenericRouterResponse::Debug(source) => Ok(*source),
                            _other => {
                                panic!("non-debug value from debug route")
                            }
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
            let Some(router) = self.get(root).and_then(|cap| Router::<T>::try_from(cap).ok())
            else {
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
    ) -> Result<(), fsandbox::CapabilityStoreError> {
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
                                current_dict.remove(current_name).unwrap();
                                current_dict.insert(current_name.into(), new_router.into())?;

                                return Ok(());
                            }
                            None => {
                                let dict = Dictionary::new();
                                current_dict.insert(
                                    current_name.into(),
                                    Capability::Dictionary(dict.clone()),
                                )?;
                                dict
                            }
                            _ => return Err(fsandbox::CapabilityStoreError::ItemNotFound),
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
        debug: bool,
        target: WeakInstanceToken,
    ) -> Result<Option<GenericRouterResponse>, RouterError> {
        let mut current_dict = self.clone();
        let num_segments = path.iter_segments().count();
        for (next_idx, next_name) in path.iter_segments().enumerate() {
            // Get the capability.
            let capability = current_dict.get(next_name);

            // The capability doesn't exist.
            let Some(capability) = capability else {
                return Ok(None);
            };

            if next_idx < num_segments - 1 {
                // Not at the end of the path yet, so there's more nesting. We expect to have found
                // a [Dictionary], or a [Dictionary] router -- traverse into this [Dictionary].
                let dict_request = request_with_dictionary_replacement(&request)?;
                match capability {
                    Capability::Dictionary(d) => {
                        current_dict = d;
                    }
                    Capability::DictionaryRouter(r) => {
                        match r.route(dict_request, target.clone()).await? {
                            Some(d) => {
                                current_dict = d;
                            }
                            None => {
                                if !debug {
                                    return Ok(Some(GenericRouterResponse::Unavailable));
                                } else {
                                    // `debug=true` was the input to this function but the call
                                    // above to [`Router::route`] used `debug=false`. Call the
                                    // router again with the same arguments but with `debug=true`
                                    // so that we return the debug info to the caller (which ought
                                    // to be [`CapabilitySource::Void`]).
                                    let dict_request =
                                        request_with_dictionary_replacement(&request)?;
                                    let source = r.route_debug(dict_request, target).await?;
                                    return Ok(Some(GenericRouterResponse::Debug(Box::new(
                                        source,
                                    ))));
                                }
                            }
                        }
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
            } else {
                // We've reached the end of our path. The last capability should have type
                // `T` or `Router<T>`.
                //
                // There's a bit of repetition here because this function supports multiple router
                // types.
                return match (capability, debug) {
                    (Capability::DictionaryRouter(r), false) => {
                        match r.route(request, target).await? {
                            Some(c) => Ok(Some(GenericRouterResponse::Capability(c.into()))),
                            None => Ok(Some(GenericRouterResponse::Unavailable)),
                        }
                    }
                    (Capability::DictionaryRouter(r), true) => {
                        let source = r.route_debug(request, target).await?;
                        Ok(Some(GenericRouterResponse::Debug(Box::new(source))))
                    }
                    (Capability::ConnectorRouter(r), false) => {
                        match r.route(request, target).await? {
                            Some(c) => Ok(Some(GenericRouterResponse::Capability(c.into()))),
                            None => Ok(Some(GenericRouterResponse::Unavailable)),
                        }
                    }
                    (Capability::ConnectorRouter(r), true) => {
                        let source = r.route_debug(request, target).await?;
                        Ok(Some(GenericRouterResponse::Debug(Box::new(source))))
                    }
                    (Capability::DataRouter(r), false) => match r.route(request, target).await? {
                        Some(c) => Ok(Some(GenericRouterResponse::Capability(c.into()))),
                        None => Ok(Some(GenericRouterResponse::Unavailable)),
                    },
                    (Capability::DataRouter(r), true) => {
                        let source = r.route_debug(request, target).await?;
                        Ok(Some(GenericRouterResponse::Debug(Box::new(source))))
                    }
                    (Capability::DirConnectorRouter(r), false) => {
                        match r.route(request, target).await? {
                            Some(c) => Ok(Some(GenericRouterResponse::Capability(c.into()))),
                            None => Ok(Some(GenericRouterResponse::Unavailable)),
                        }
                    }
                    (Capability::DirConnectorRouter(r), true) => {
                        let source = r.route_debug(request, target).await?;
                        Ok(Some(GenericRouterResponse::Debug(Box::new(source))))
                    }
                    (_other, true) => {
                        // This is a debug route, and we've found a non-router capability. We must
                        // return debug information for the debug route, and the only reason there
                        // would be a non-router capability in a dictionary would be if a user
                        // created one, so we can safely report that this was a remotely created
                        // capability.
                        let remoted_at_moniker = match moniker {
                            ExtendedMoniker::ComponentInstance(m) => m.clone(),
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
                        return Ok(Some(GenericRouterResponse::Debug(
                            CapabilitySource::RemotedAt(RemotedAtSource {
                                moniker: remoted_at_moniker,
                                type_name,
                            })
                            .try_into()
                            .expect("failed to serialize capability source"),
                        )));
                    }
                    (other, false) => Ok(Some(GenericRouterResponse::Capability(other))),
                };
            }
        }
        unreachable!("get_with_request: All cases are handled in the loop");
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
    preexisting_router: Router<Dictionary>,
    path: RelativePath,
    capability: Capability,
}

#[async_trait]
impl Routable<Dictionary> for AdditiveDictionaryRouter {
    async fn route(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<Option<Dictionary>, RouterError> {
        let dictionary = match self.preexisting_router.route(request, target).await {
            Ok(Some(dictionary)) => dictionary.shallow_copy().unwrap(),
            other_response => return other_response,
        };
        let _ = dictionary.insert_capability(&self.path, self.capability.clone());
        Ok(Some(dictionary))
    }

    async fn route_debug(
        &self,
        request: RouteRequest,
        target: WeakInstanceToken,
    ) -> Result<CapabilitySource, RouterError> {
        self.preexisting_router.route_debug(request, target).await
    }
}
